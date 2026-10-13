//! Cask artifact-scope resolution + per-context cask body generation.
//!
//! Split from `cask.rs`: the template const, `CaskParams`, and the pure
//! render helpers stay there; the artifact-scope enumeration
//! ([`CaskArtifactScope`]), multi-platform block builder
//! ([`build_cask_platform_blocks`]), the per-context cask generator
//! ([`generate_cask_from_context`]) and the top-level artifact lookups live
//! here.

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::template::ruby_escape_str;
use anyhow::Result;

use super::cask::{
    CaskArchEntry, CaskBinaryEntry, CaskParams, CaskPlatformBlock, UNIVERSAL_ARCH_BLOCK,
    cask_name_for, generate_cask, render_additional_url_params, render_alternative_names,
    render_generate_completions, render_uninstall_block, render_zap_block, split_alternative_names,
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
    /// the optional `ids:` filter (mirrors [`has_top_level_cask_artifact`]).
    TopLevel { ids: Option<&'a [String]> },
}

impl CaskArtifactScope<'_> {
    /// The artifacts of `kind` in this scope, with the universal-binary
    /// filter ([`Artifact::only_replacing_unibins`](anodizer_core::artifact::Artifact::only_replacing_unibins))
    /// applied — identical to
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
///
/// A universal macOS artifact (`darwin-universal`, arch `all`) stands alone
/// as a flat `sha256` / `url` in `on_macos` when no per-arch macOS artifact is
/// in scope, and fills the one empty `on_arm` / `on_intel` slot when its build
/// id matches the per-arch artifact's. Otherwise it is left out and the skip
/// is logged at verbose.
pub(super) fn build_cask_platform_blocks(
    ctx: &Context,
    scope: &CaskArtifactScope<'_>,
    version: &str,
    url_template: Option<&str>,
    error_scope_label: &str,
    log: &anodizer_core::log::StageLogger,
) -> Result<Vec<CaskPlatformBlock>> {
    use std::collections::BTreeMap;
    // Each entry keeps its artifact's build id, which decides whether the
    // universal artifact may fill a slot beside it.
    let mut os_map: BTreeMap<String, Vec<(CaskArchEntry, Option<String>)>> = BTreeMap::new();
    // The first universal artifact per OS with its name and build id, placed
    // once every per-arch slot is known.
    let mut universal: BTreeMap<String, (String, Option<String>, CaskArchEntry)> = BTreeMap::new();
    for kind in &CASK_DOWNLOAD_KINDS {
        for art in scope.artifacts_of_kind(ctx, *kind) {
            let target = art.target.as_deref().unwrap_or("");
            // A target-less artifact reaching the `on_macos` block is a disk
            // image, which serves every Mac.
            let (os, arch) = if art.target.is_none() {
                ("darwin".to_string(), "all".to_string())
            } else {
                anodizer_core::target::map_target(target)
            };
            let os_block = if is_macos_cask_download(art) {
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
            } else if arch == "all" && os_block == "macos" {
                UNIVERSAL_ARCH_BLOCK
            } else {
                continue;
            };
            // Dedup is per-OS: a darwin `intel` entry must not suppress a
            // linux `intel` entry. Only skip when THIS os_block already
            // holds an entry for THIS arch_block (the first-kind-wins
            // precedence: DiskImage > Archive > UploadableBinary).
            let taken = if arch_block == UNIVERSAL_ARCH_BLOCK {
                universal.contains_key(os_block)
            } else {
                os_map
                    .get(os_block)
                    .is_some_and(|arches| arches.iter().any(|(e, _)| e.arch_block == arch_block))
            };
            if taken {
                continue;
            }
            let url = if let Some(tmpl) = url_template {
                crate::util::render_url_template_with_ctx(
                    ctx,
                    tmpl,
                    art.name(),
                    version,
                    crate::util::UrlTarget::of(art, &os, &arch),
                )
            } else if let Some(u) = art.metadata.get("url") {
                u.clone()
            } else {
                continue;
            };
            // Escape before substituting: `#{version}` is Ruby
            // interpolation anodizer emits on purpose, everything else in
            // the URL must reach the file as a literal.
            let url = ruby_escape_str(&url).replace(version, "#{version}");
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
            let entry = CaskArchEntry {
                arch_block: arch_block.to_string(),
                url,
                sha256,
            };
            let id = art.metadata.get("id").cloned();
            if arch_block == UNIVERSAL_ARCH_BLOCK {
                universal.insert(os_block.to_string(), (art.name().to_string(), id, entry));
            } else {
                os_map
                    .entry(os_block.to_string())
                    .or_default()
                    .push((entry, id));
            }
        }
    }
    // A flat `url` beside `on_arm`/`on_intel` urls in one `on_macos` block
    // would give Homebrew two downloads for one host, so the universal
    // artifact only ever stands in for a slot nothing else filled. It fills
    // one beside a per-arch artifact of the same build only: a universal
    // binary of another build is a different program.
    // (`universal_binaries.replace: false` marks its universal artifact
    // `replaces: false`, which `artifacts_of_kind` already leaves out.)
    for (os_block, (name, id, entry)) in universal {
        let arches = os_map.entry(os_block).or_default();
        let missing: Vec<&str> = ["arm", "intel"]
            .into_iter()
            .filter(|slot| !arches.iter().any(|(e, _)| e.arch_block == *slot))
            .collect();
        match missing.as_slice() {
            [_, _] => arches.push((entry, id)),
            [slot] => {
                let (filled, filled_id) = &arches[0];
                let arch = &filled.arch_block;
                // Two artifacts without an id are not known to be one build,
                // so a missing id on either side never fills the slot.
                match (&id, filled_id) {
                    (Some(own), Some(other)) if own == other => arches.push((
                        CaskArchEntry {
                            arch_block: (*slot).to_string(),
                            ..entry
                        },
                        id,
                    )),
                    (None, _) => log.verbose(&format!(
                        "skipped universal artifact '{name}' — it has no id to match against \
                         the macOS {arch} download"
                    )),
                    (Some(_), None) => log.verbose(&format!(
                        "skipped universal artifact '{name}' — the macOS {arch} download has no id"
                    )),
                    (Some(own), Some(other)) => log.verbose(&format!(
                        "skipped universal artifact '{name}' — build id '{own}' does not match \
                         '{other}', the build of the macOS {arch} download"
                    )),
                }
            }
            _ => log.verbose(&format!(
                "skipped universal artifact '{name}' — both macOS architectures have their own download"
            )),
        }
    }
    Ok(os_map
        .into_iter()
        // The emitted order of OS and arch blocks is decided where the cask
        // is rendered (`cask::generate_cask`), so it holds for every caller.
        .map(|(os_block, arches)| CaskPlatformBlock {
            os_block,
            arches: arches.into_iter().map(|(e, _)| e).collect(),
        })
        .collect())
}

/// The one macOS download of a cask whose blocks hold a single entry, which
/// renders as the flat top-level `url` / `sha256`. `None` when that entry is
/// not macOS or the blocks hold none.
pub(super) fn single_macos_download(blocks: &[CaskPlatformBlock]) -> Option<&CaskArchEntry> {
    match blocks {
        [only] if only.os_block == "macos" && only.arches.len() == 1 => only.arches.first(),
        _ => None,
    }
}

/// Number of OS x arch downloads the blocks carry. More than one renders the
/// per-OS / per-arch body; one renders flat.
pub(super) fn download_count(blocks: &[CaskPlatformBlock]) -> usize {
    blocks.iter().map(|p| p.arches.len()).sum()
}

/// The logger a publisher renders its cask through. Emission validation
/// renders every cask first and prints the render's `-v` lines, so the
/// publisher keeps only its warnings and status lines. A `--publish-only`
/// run has no emission validation, and there the publisher prints them.
pub(super) fn publisher_render_log(ctx: &Context, log: &StageLogger) -> StageLogger {
    if ctx.is_publish_only() {
        log.clone()
    } else {
        log.capped_at(anodizer_core::log::Verbosity::Normal)
    }
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
    let cask_name = cask_name_for(cask_cfg.name.as_deref().unwrap_or(crate_name));

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
        log,
    )?;

    // A single download renders as the flat top-level `url`/`sha256`:
    // per-OS and per-arch blocks around one entry only add nesting.
    let use_platforms = download_count(&platform_blocks) > 1;
    let (url, sha256) = if use_platforms {
        (String::new(), String::new())
    } else {
        let only = single_macos_download(&platform_blocks).ok_or_else(|| {
            anyhow::anyhow!(
                "homebrew cask: no macOS artifact with a url found for '{}'; set url_template \
                 or ensure release uploads set artifact URLs",
                crate_name
            )
        })?;
        (only.url.clone(), only.sha256.clone())
    };

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

    // Build dependency directives and the conflicting cask names
    let cask_depends = super::formula::build_depends_directives(cask_cfg.dependencies.as_deref());
    let cask_conflicts = super::cask::conflicting_casks(cask_cfg.conflicts.as_deref());

    // Each `uninstall` / `zap` sub-key (`launchctl`, `quit`, `login_item`,
    // `delete`, `trash`) renders as its own keyed argument: Homebrew reads
    // each key as a different removal method, so the entries of one key
    // cannot be listed under another.
    let uninstall_block = render_uninstall_block(cask_cfg.uninstall.as_ref());
    let zap_block = render_zap_block(cask_cfg.zap.as_ref());

    // Pre-rendered Ruby kwargs continuation for the `url` line. When the
    // cask config does not set `url:` (a `url_template` was used instead),
    // there are no kwargs to splice — empty string emits `url "..."` with
    // no trailing kwargs, which is valid Cask DSL.
    let url_extras_top = cask_cfg
        .url
        .as_ref()
        .map(|u| render_additional_url_params(u, super::cask::URL_ARGS_INDENT))
        .unwrap_or_default();
    let url_extras_arch = cask_cfg
        .url
        .as_ref()
        .map(|u| render_additional_url_params(u, super::cask::ARCH_URL_ARGS_INDENT))
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
    let (alias_alts, versioned_alts) = split_alternative_names(&rendered_alts, &cask_name);

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
        name: &cask_name,
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
        cask_name,
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
/// The artifact kinds a cask downloads, in the precedence
/// [`build_cask_platform_blocks`] fills a slot with.
const CASK_DOWNLOAD_KINDS: [ArtifactKind; 3] = [
    ArtifactKind::DiskImage,
    ArtifactKind::Archive,
    ArtifactKind::UploadableBinary,
];

/// True when `art` is a download [`build_cask_platform_blocks`] places in the
/// `on_macos` block: a cask download kind built for a `*-apple-darwin`
/// target, or a disk image built without a target, which is a macOS-only
/// format and so serves every Mac.
pub(super) fn is_macos_cask_download(art: &Artifact) -> bool {
    match art.target.as_deref() {
        // `is_macos` matches genuine `*-apple-darwin` only: `*-apple-ios`,
        // `-watchos` and `-tvos` carry nothing `brew` can install.
        Some(target) => {
            CASK_DOWNLOAD_KINDS.contains(&art.kind) && anodizer_core::target::is_macos(target)
        }
        None => art.kind == ArtifactKind::DiskImage,
    }
}

/// True when the release has a macOS cask download for the top-level
/// `homebrew_casks:` publisher, across every crate, narrowed by the optional
/// `ids:` filter.
pub(super) fn has_top_level_cask_artifact(ctx: &Context, ids: Option<&[String]>) -> bool {
    let scope = CaskArtifactScope::TopLevel { ids };
    CASK_DOWNLOAD_KINDS.iter().any(|kind| {
        scope
            .artifacts_of_kind(ctx, *kind)
            .into_iter()
            .any(is_macos_cask_download)
    })
}

/// True when `crate_name` has a macOS cask download in scope, which
/// [`build_cask_platform_blocks`] places in the `on_macos` block. It asks the
/// same [`is_macos_cask_download`] question, so a validator can gate a
/// not-applicable skip on artifact presence, then call the render and
/// propagate any `Err` (a present-but-broken artifact, missing url/sha256),
/// instead of collapsing every render `bail!` to "skip".
pub(crate) fn crate_has_macos_cask_artifact(ctx: &Context, crate_name: &str) -> bool {
    let scope = CaskArtifactScope::Crate { crate_name };
    CASK_DOWNLOAD_KINDS.iter().any(|kind| {
        scope
            .artifacts_of_kind(ctx, *kind)
            .into_iter()
            .any(is_macos_cask_download)
    })
}
