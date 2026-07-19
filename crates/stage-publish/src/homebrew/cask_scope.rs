//! Cask artifact-scope resolution + per-context cask body generation.
//!
//! Split from `cask.rs`: the template const, `CaskParams`, and the pure
//! render helpers stay there; the artifact-scope enumeration
//! ([`CaskArtifactScope`]), multi-platform block builder
//! ([`build_cask_platform_blocks`]), the per-context cask generator
//! ([`generate_cask_from_context`]) and the top-level artifact lookups live
//! here.

use anodizer_core::context::Context;
use anodizer_core::template::ruby_escape_str;
use anyhow::Result;

use super::cask::{
    CaskArchEntry, CaskBinaryEntry, CaskParams, CaskPlatformBlock, generate_cask,
    render_additional_url_params, render_alternative_names, render_generate_completions,
    render_uninstall_block, render_zap_block, split_alternative_names,
};

// ---------------------------------------------------------------------------
// generate_cask_content – shared helper for cask file generation
// ---------------------------------------------------------------------------

/// Intermediate result from cask content generation: the rendered cask file
/// string, the cask name used as the filename stem, and any versioned
/// alternative-name files to emit alongside (one extra `.rb` per entry).
pub(crate) struct CaskGenResult {
    pub(crate) content: String,
    pub(crate) cask_name: String,
    /// Tuples of (filename-stem, file-body) for each versioned alt-name
    /// (e.g. `myapp@1.2.3`). Empty when no alt-name templated to a
    /// versioned form. Each body is the same as `content` but with the
    /// `cask "<name>"` header re-keyed to the alt-name so
    /// `brew install <alt>` resolves to the version-pinned cask.
    pub(crate) versioned_files: Vec<(String, String)>,
}

/// Artifact-enumeration scope for [`build_cask_platform_blocks`].
///
/// The per-crate cask publisher gathers only the owning crate's artifacts
/// ([`Self::Crate`]); the top-level `homebrew_casks:` publisher gathers darwin
/// and linux artifacts across the whole release, optionally narrowed by an
/// `ids:` filter ([`Self::TopLevel`]). Both then map each artifact's target to
/// an `on_macos`/`on_linux` × `on_intel`/`on_arm` slot — that mapping is shared.
pub(super) enum CaskArtifactScope<'a> {
    /// Per-crate scope: only `crate_name`'s artifacts of the given kind.
    Crate { crate_name: &'a str },
    /// Top-level scope: every crate's artifacts of the given kind, narrowed by
    /// the optional `ids:` filter (mirrors [`find_top_level_cask_artifact`]).
    TopLevel { ids: Option<&'a [String]> },
}

impl CaskArtifactScope<'_> {
    /// The artifacts of `kind` in this scope, with the universal-binary
    /// filter ([`Artifact::only_replacing_unibins`]) applied — identical to
    /// what the per-crate and top-level single-artifact lookups use, so a
    /// `universal_binaries.replace: false` release keeps both the universal
    /// and per-arch entries.
    fn artifacts_of_kind<'c>(
        &self,
        ctx: &'c Context,
        kind: anodizer_core::artifact::ArtifactKind,
    ) -> Vec<&'c anodizer_core::artifact::Artifact> {
        match self {
            CaskArtifactScope::Crate { crate_name } => ctx
                .artifacts
                .by_kind_and_crate(kind, crate_name)
                .into_iter()
                .filter(|a| a.only_replacing_unibins())
                .collect(),
            CaskArtifactScope::TopLevel { ids } => ctx
                .artifacts
                .by_kind(kind)
                .into_iter()
                .filter(|a| a.only_replacing_unibins())
                .filter(|a| anodizer_core::artifact::matches_id_filter(a, *ids))
                .collect(),
        }
    }
}

/// Build the per-platform `on_macos` / `on_linux` cask blocks — each carrying
/// one `on_intel` / `on_arm` entry per architecture present in the release —
/// from the artifacts in `scope`.
///
/// This is the single source of truth for multi-arch cask emission, shared by
/// the per-crate publisher ([`generate_cask_from_context`]) and the top-level
/// `homebrew_casks:` publisher. Each architecture gets its OWN url + sha256 so
/// `brew install` serves every Mac (and Linux) host the binary built for its
/// architecture — emitting a single flat url for a multi-arch release would
/// ship one architecture's binary to all hosts.
///
/// Kind precedence is `DiskImage` > `Archive` > `UploadableBinary`; the first
/// kind that supplies a given OS×arch slot wins. `url_template`, when set,
/// renders each artifact's url; otherwise the artifact's `metadata["url"]` is
/// used. The version substring in each url is rewritten to `#{version}` for
/// Homebrew auto-update. A multi-platform block with an empty `sha256 ""` line
/// fails `brew install`, so a missing sha256 is a hard error.
pub(super) fn build_cask_platform_blocks(
    ctx: &Context,
    scope: &CaskArtifactScope<'_>,
    version: &str,
    url_template: Option<&str>,
    error_scope_label: &str,
) -> Result<Vec<CaskPlatformBlock>> {
    use std::collections::BTreeMap;
    let kinds = [
        anodizer_core::artifact::ArtifactKind::DiskImage,
        anodizer_core::artifact::ArtifactKind::Archive,
        anodizer_core::artifact::ArtifactKind::UploadableBinary,
    ];
    let mut os_map: BTreeMap<String, Vec<CaskArchEntry>> = BTreeMap::new();
    for kind in &kinds {
        for art in scope.artifacts_of_kind(ctx, *kind) {
            let target = art.target.as_deref().unwrap_or("");
            let (os, arch) = anodizer_core::target::map_target(target);
            // `is_macos` (genuine `*-apple-darwin` only), NOT `os == "darwin"`:
            // map_target folds `*-apple-watchos`/`-tvos` into os="darwin", but a
            // watchOS archive dropped into the `on_macos` block is a
            // non-installable cask `url`. Mirrors the formula's `is_macos` gate.
            let os_block = if anodizer_core::target::is_macos(target) {
                "macos"
            } else if anodizer_core::target::is_linux(target) {
                "linux"
            } else {
                continue;
            };
            let arch_block = if arch == "amd64" || arch == "386" {
                "intel"
            } else if arch == "arm64" {
                "arm"
            } else {
                continue;
            };
            // Dedup is per-OS: a darwin `intel` entry must not suppress a
            // linux `intel` entry. Only skip when THIS os_block already
            // holds an entry for THIS arch_block (the first-kind-wins
            // precedence: DiskImage > Archive > UploadableBinary).
            if os_map
                .get(os_block)
                .is_some_and(|arches| arches.iter().any(|e| e.arch_block == arch_block))
            {
                continue;
            }
            let url = if let Some(tmpl) = url_template {
                crate::util::render_url_template_with_ctx(
                    ctx,
                    tmpl,
                    art.name(),
                    version,
                    &arch,
                    &os,
                )
            } else if let Some(u) = art.metadata.get("url") {
                u.clone()
            } else {
                continue;
            };
            let url = url.replace(version, "#{version}");
            let sha256 = art
                .metadata
                .get("sha256")
                .cloned()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "homebrew cask: artifact '{}' (os={}, arch={}) for {} \
                         is missing required sha256 metadata. A multi-platform cask \
                         block with an empty `sha256 \"\"` line fails `brew style` and \
                         aborts `brew install` (Homebrew verifies the SHA before \
                         extracting). This indicates the artifacts.json catalog dropped \
                         the entry's sha256 before the publish stage. Re-run with \
                         `task release` from a clean dist/ and verify dist/artifacts.json \
                         carries metadata.sha256 for every macOS/Linux artifact.",
                        art.name(),
                        os_block,
                        arch_block,
                        error_scope_label,
                    )
                })?;
            os_map
                .entry(os_block.to_string())
                .or_default()
                .push(CaskArchEntry {
                    arch_block: arch_block.to_string(),
                    url,
                    sha256,
                });
        }
    }
    Ok(os_map
        .into_iter()
        .map(|(os_block, mut arches)| {
            // Pin per-arch order by `arch_block` so emission is byte-stable
            // regardless of the upstream artifact catalog's insertion order
            // (BTreeMap groups Archive targets but DiskImage / UploadableBinary
            // kinds carry no such guarantee). Ascending: "arm" precedes "intel".
            arches.sort_by(|a, b| a.arch_block.cmp(&b.arch_block));
            CaskPlatformBlock { os_block, arches }
        })
        .collect())
}

/// Generate a Homebrew Cask `.rb` file string from the project context.
///
/// Renders the cask's `url`/`sha256`/artifact stanzas (`binary`, `app`, `pkg`)
/// and version block, returning a [`CaskGenResult`] with the finished `.rb`
/// source, the resolved cask name, and any version-pinned alt-name files.
pub(super) fn generate_cask_from_context(
    ctx: &Context,
    crate_name: &str,
    hb_cfg: &anodizer_core::config::HomebrewConfig,
    cask_cfg: &anodizer_core::config::HomebrewCaskConfig,
    log: &anodizer_core::log::StageLogger,
) -> Result<CaskGenResult> {
    let version = ctx.version();
    let cask_name = cask_cfg.name.as_deref().unwrap_or(crate_name);

    let url_template = cask_cfg
        .url_template
        .as_deref()
        .or(hb_cfg.url_template.as_deref());

    // Build per-platform `on_macos` / `on_linux` blocks, each carrying one
    // `on_arm` / `on_intel` entry per architecture present in the release.
    let platform_blocks = build_cask_platform_blocks(
        ctx,
        &CaskArtifactScope::Crate { crate_name },
        &version,
        url_template,
        &format!("crate '{}'", crate_name),
    )?;

    // Find primary artifact for fallback single-platform behavior
    let primary_artifact = ctx
        .artifacts
        .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::DiskImage, crate_name)
        .into_iter()
        .filter(|a| a.only_replacing_unibins())
        .find(|a| {
            a.target
                .as_deref()
                .map(anodizer_core::target::is_macos)
                .unwrap_or(true)
        })
        .or_else(|| {
            ctx.artifacts
                .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Archive, crate_name)
                .into_iter()
                .filter(|a| a.only_replacing_unibins())
                .find(|a| {
                    a.target
                        .as_deref()
                        .map(anodizer_core::target::is_macos)
                        .unwrap_or(false)
                })
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "homebrew cask: no macOS artifact (DiskImage or Archive) found for '{}'",
                crate_name
            )
        })?;

    // Single-platform URL/SHA for backwards compatibility (used when no multi-platform blocks)
    let url = if let Some(tmpl) = url_template {
        let target = primary_artifact.target.as_deref().unwrap_or("");
        let (os, arch) = anodizer_core::target::map_target(target);
        crate::util::render_url_template_with_ctx(
            ctx,
            tmpl,
            primary_artifact.name(),
            &version,
            &arch,
            &os,
        )
    } else {
        primary_artifact.metadata.get("url").cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "homebrew cask: artifact for '{}' has no 'url' metadata; set url_template or ensure release uploads set artifact URLs",
                crate_name
            )
        })?
    };
    let url = url.replace(&version, "#{version}");

    let sha256 = primary_artifact
        .metadata
        .get("sha256")
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "homebrew cask: artifact for '{}' has no 'sha256' metadata",
                crate_name
            )
        })?;

    // Use multi-platform blocks when there's more than one platform entry
    let use_platforms = platform_blocks
        .iter()
        .map(|p| p.arches.len())
        .sum::<usize>()
        > 1;

    let display_name = cask_cfg.name.as_deref().unwrap_or(crate_name);
    let empty_vec: Vec<String> = Vec::new();

    // Map config-side `HomebrewCaskBinary` entries (untagged enum: bare string
    // OR `{ name, target }`) into the template-side `CaskBinaryEntry` shape so
    // the template renders `binary "<n>"` for the bare form and
    // `binary "<n>", target: "<t>"` when the rename target is set.
    //
    // When neither `binaries:` nor `app:` is configured, default to a single
    // `binary "<cask_name>"` so the cask declares at least one artifact stanza
    // — a cask with no artifact directive fails `brew audit` and installs
    // nothing. Mirrors the top-level `homebrew_casks:` path's default.
    let mut cask_binaries: Vec<CaskBinaryEntry> = cask_cfg
        .binaries
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|b| CaskBinaryEntry {
            name: b.name().to_string(),
            target: b.target().map(str::to_string),
        })
        .collect();
    if cask_binaries.is_empty() && cask_cfg.app.is_none() {
        cask_binaries.push(CaskBinaryEntry {
            name: cask_name.to_string(),
            target: None,
        });
    }

    // Build dependency and conflict directive strings for the template
    let cask_depends: Vec<String> = cask_cfg
        .dependencies
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|d| {
            if let Some(ref c) = d.cask {
                format!("cask: \"{}\"", ruby_escape_str(c))
            } else if let Some(ref f) = d.formula {
                format!("formula: \"{}\"", ruby_escape_str(f))
            } else {
                String::new()
            }
        })
        .filter(|s| !s.is_empty())
        .collect();
    let cask_conflicts: Vec<String> = cask_cfg
        .conflicts
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|c| {
            if let Some(ref cask) = c.cask {
                format!("cask: \"{}\"", ruby_escape_str(cask))
            } else if let Some(ref formula) = c.formula {
                format!("formula: \"{}\"", ruby_escape_str(formula))
            } else {
                String::new()
            }
        })
        .filter(|s| !s.is_empty())
        .collect();

    // Each `uninstall` / `zap` sub-key (`launchctl`, `quit`, `login_item`,
    // `delete`, `trash`) renders as its own keyed Ruby array. The prior
    // shape hard-coded `zap trash: [...]` and shoved every sub-directive
    // into that array, producing broken Ruby for non-trash directives.
    let uninstall_block = render_uninstall_block(cask_cfg.uninstall.as_ref());
    let zap_block = render_zap_block(cask_cfg.zap.as_ref());

    // Pre-rendered Ruby kwargs continuation for the `url` line. When the
    // cask config does not set `url:` (a `url_template` was used instead),
    // there are no kwargs to splice — empty string emits `url "..."` with
    // no trailing kwargs, which is valid Cask DSL.
    let url_extras_top = cask_cfg
        .url
        .as_ref()
        .map(|u| render_additional_url_params(u, "      "))
        .unwrap_or_default();
    let url_extras_arch = cask_cfg
        .url
        .as_ref()
        .map(|u| render_additional_url_params(u, "        "))
        .unwrap_or_default();

    // Pre-render every `alternative_names` entry through the user's
    // template engine. Entries like `myproject@{{ .Version }}` are
    // templated, not literal. Without this pass the rendered Ruby would
    // carry the unresolved `{{ .Version }}` substring and `brew style`
    // would reject it.
    let rendered_alts = render_alternative_names(
        ctx,
        cask_cfg.alternative_names.as_deref().unwrap_or(&empty_vec),
    )?;
    let (alias_alts, versioned_alts) = split_alternative_names(&rendered_alts, cask_name);

    // Template-render the user-supplied free-text fields here — the scope with
    // the real `Context`+`log` — so a value like `caveats: "see {{ .Tag }}"`
    // resolves before reaching `generate_cask` (which holds only a bare
    // `tera::Context`). Mirrors the formula publisher's `resolve_homebrew_metadata`.
    // Fallback chain (per-cask → per-formula → project metadata) is resolved
    // first, then the chosen value rendered.
    let homepage = cask_cfg
        .homepage
        .as_deref()
        .or(hb_cfg.homepage.as_deref())
        .or_else(|| ctx.config.meta_homepage_for(crate_name))
        .map(|s| crate::util::render_or_warn(ctx, log, "cask.homepage", s))
        .transpose()?;
    let description = cask_cfg
        .description
        .as_deref()
        .or(hb_cfg.description.as_deref())
        .or_else(|| ctx.config.meta_description_for(crate_name))
        .map(|s| crate::util::render_or_warn(ctx, log, "cask.description", s))
        .transpose()?;
    let caveats = cask_cfg
        .caveats
        .as_deref()
        .map(|s| crate::util::render_or_warn(ctx, log, "cask.caveats", s))
        .transpose()?;
    let custom_block = cask_cfg
        .custom_block
        .as_deref()
        .map(|s| crate::util::render_or_warn(ctx, log, "cask.custom_block", s))
        .transpose()?;
    let app = cask_cfg
        .app
        .as_deref()
        .map(|s| crate::util::render_or_warn(ctx, log, "cask.app", s))
        .transpose()?;
    let service = cask_cfg
        .service
        .as_deref()
        .map(|s| crate::util::render_or_warn(ctx, log, "cask.service", s))
        .transpose()?;

    let params = CaskParams {
        name: cask_name,
        display_name,
        alternative_names: &alias_alts,
        version: &version,
        sha256: &sha256,
        url: &url,
        url_extras: &url_extras_top,
        url_extras_indented: &url_extras_arch,
        homepage: homepage.as_deref(),
        description: description.as_deref(),
        app: app.as_deref(),
        binaries: &cask_binaries,
        caveats: caveats.as_deref(),
        zap_block: &zap_block,
        uninstall_block: &uninstall_block,
        custom_block: custom_block.as_deref(),
        service: service.as_deref(),
        livecheck: super::formula::render_livecheck(cask_cfg.livecheck.as_ref(), log),
        manpages: cask_cfg.manpages.as_deref().unwrap_or(&empty_vec),
        completions_bash: cask_cfg
            .completions
            .as_ref()
            .and_then(|c| c.bash.as_deref()),
        completions_zsh: cask_cfg.completions.as_ref().and_then(|c| c.zsh.as_deref()),
        completions_fish: cask_cfg
            .completions
            .as_ref()
            .and_then(|c| c.fish.as_deref()),
        platforms: if use_platforms {
            platform_blocks
        } else {
            Vec::new()
        },
        depends_on: &cask_depends,
        conflicts_with: &cask_conflicts,
        preflight: cask_cfg
            .hooks
            .as_ref()
            .and_then(|h| h.pre.as_ref())
            .and_then(|p| p.install.as_deref()),
        postflight: cask_cfg
            .hooks
            .as_ref()
            .and_then(|h| h.post.as_ref())
            .and_then(|p| p.install.as_deref()),
        uninstall_preflight: cask_cfg
            .hooks
            .as_ref()
            .and_then(|h| h.pre.as_ref())
            .and_then(|p| p.uninstall.as_deref()),
        uninstall_postflight: cask_cfg
            .hooks
            .as_ref()
            .and_then(|h| h.post.as_ref())
            .and_then(|p| p.uninstall.as_deref()),
        generate_completions: cask_cfg
            .generate_completions_from_executable
            .as_ref()
            .and_then(render_generate_completions),
    };

    let content = generate_cask(&params)?;
    // Final-text chokepoint: a residual `{{ … }}` means a config field escaped
    // rendering — fail strict, warn lenient — before the cask is written or
    // pushed. Ruby `#{}` interpolation is not scanned, so `#{version}` is safe.
    crate::util::guard_no_unrendered(ctx, log, "homebrew cask", &content)?;

    // For each versioned alt-name, emit a second .rb whose body re-keys
    // the `cask "<name>" do` header to the alt-name and drops the
    // (cosmetic) `alternative_names` aliases — the alt-name's own file
    // does not need to advertise the others. The versioned cask is the
    // pin: it points at the same URL/sha256 as the primary cask body, so
    // a user who pinned to `myapp@1.2.3` can downgrade and stay there.
    let mut versioned_files: Vec<(String, String)> = Vec::with_capacity(versioned_alts.len());
    for alt in &versioned_alts {
        let alt_params = CaskParams {
            name: alt,
            display_name: alt,
            // Inside the versioned cask file, don't carry the alias list;
            // the file's identity IS the alt-name.
            alternative_names: &[],
            ..clone_cask_params(&params)
        };
        let body = generate_cask(&alt_params)?;
        crate::util::guard_no_unrendered(ctx, log, "homebrew cask", &body)?;
        versioned_files.push((alt.clone(), body));
    }

    Ok(CaskGenResult {
        content,
        cask_name: cask_name.to_string(),
        versioned_files,
    })
}

/// Borrow-shaped clone of [`CaskParams`] used when re-rendering the same
/// cask body under a different `name` / `display_name` (the versioned
/// alt-name path). All references re-bind to the source struct's
/// lifetime so the alt-name's own slices stay independent.
pub(super) fn clone_cask_params<'a>(p: &'a CaskParams<'a>) -> CaskParams<'a> {
    CaskParams {
        name: p.name,
        display_name: p.display_name,
        alternative_names: p.alternative_names,
        version: p.version,
        sha256: p.sha256,
        url: p.url,
        url_extras: p.url_extras,
        url_extras_indented: p.url_extras_indented,
        homepage: p.homepage,
        description: p.description,
        app: p.app,
        binaries: p.binaries,
        caveats: p.caveats,
        zap_block: p.zap_block,
        uninstall_block: p.uninstall_block,
        custom_block: p.custom_block,
        service: p.service,
        livecheck: p.livecheck.clone(),
        manpages: p.manpages,
        completions_bash: p.completions_bash,
        completions_zsh: p.completions_zsh,
        completions_fish: p.completions_fish,
        platforms: p.platforms.clone(),
        depends_on: p.depends_on,
        conflicts_with: p.conflicts_with,
        preflight: p.preflight,
        postflight: p.postflight,
        uninstall_preflight: p.uninstall_preflight,
        uninstall_postflight: p.uninstall_postflight,
        generate_completions: p.generate_completions.clone(),
    }
}
/// Find a macOS artifact for top-level cask config.
/// Searches all artifacts (not per-crate) with optional ID filtering.
pub(super) fn find_top_level_cask_artifact<'a>(
    ctx: &'a Context,
    ids: Option<&[String]>,
) -> Option<&'a anodizer_core::artifact::Artifact> {
    let filter = |a: &&anodizer_core::artifact::Artifact| {
        if !anodizer_core::artifact::matches_id_filter(a, ids) {
            return false;
        }
        // `is_macos` (genuine `*-apple-darwin` only), NOT the broad
        // `contains("apple")`: the latter also selects `*-apple-ios`/`-watchos`/
        // `-tvos`, which carry no `brew`-installable binary and would land in the
        // cask's `url`/`sha256` (a 404-class cask install). Mirrors the formula.
        a.target
            .as_deref()
            .map(anodizer_core::target::is_macos)
            .unwrap_or(false)
    };

    // Prefer DiskImage (with OnlyReplacingUnibins filter)
    ctx.artifacts
        .by_kind(anodizer_core::artifact::ArtifactKind::DiskImage)
        .into_iter()
        .filter(|a| a.only_replacing_unibins())
        .find(|a| filter(a))
        .or_else(|| {
            // Fall back to Archive
            ctx.artifacts
                .by_kind(anodizer_core::artifact::ArtifactKind::Archive)
                .into_iter()
                .filter(|a| a.only_replacing_unibins())
                .find(|a| filter(a))
        })
        .or_else(|| {
            // Fall back to UploadableBinary
            ctx.artifacts
                .by_kind(anodizer_core::artifact::ArtifactKind::UploadableBinary)
                .into_iter()
                .filter(|a| a.only_replacing_unibins())
                .find(|a| filter(a))
        })
}

/// True when `crate_name` has a macOS (darwin) `DiskImage` or `Archive`
/// artifact in scope — i.e. [`generate_cask_from_context`]'s primary-artifact
/// lookup will succeed. Mirrors that lookup's darwin predicate exactly so a
/// validator can gate a not-applicable skip on artifact PRESENCE, then call the
/// render and propagate any `Err` (a present-but-broken artifact, missing
/// url/sha256), instead of collapsing every render `bail!` to "skip".
pub(crate) fn crate_has_macos_cask_artifact(ctx: &Context, crate_name: &str) -> bool {
    // A DiskImage is a macOS-only format, so a target-less one counts as darwin
    // (`unwrap_or(true)`) — exactly the primary-artifact lookup's predicate. An
    // Archive must name a darwin/macos target (`unwrap_or(false)`).
    let dmg_is_darwin = |a: &anodizer_core::artifact::Artifact| {
        a.target
            .as_deref()
            .map(anodizer_core::target::is_macos)
            .unwrap_or(true)
    };
    let archive_is_darwin = |a: &anodizer_core::artifact::Artifact| {
        a.target
            .as_deref()
            .map(anodizer_core::target::is_macos)
            .unwrap_or(false)
    };
    ctx.artifacts
        .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::DiskImage, crate_name)
        .iter()
        .filter(|a| a.only_replacing_unibins())
        .any(|a| dmg_is_darwin(a))
        || ctx
            .artifacts
            .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Archive, crate_name)
            .iter()
            .filter(|a| a.only_replacing_unibins())
            .any(|a| archive_is_darwin(a))
}
