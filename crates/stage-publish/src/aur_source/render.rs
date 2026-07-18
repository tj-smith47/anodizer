use anodizer_core::config::AurSourceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::aur::AurRendered;
use crate::util;

use super::*;

/// A rendered source-AUR entry plus the identifiers the write/push path needs.
pub(crate) struct AurSourceRender {
    /// The rendered `PKGBUILD` + `.SRCINFO`.
    pub(crate) rendered: AurRendered,
    /// Resolved AUR package name (post-template, post `-bin` strip when the
    /// caller requested it).
    pub(crate) pkg_name: String,
    /// `<pkg_name>.install` — the filename the optional `install:` content is
    /// written to and the PKGBUILD `install=` line references.
    pub(crate) install_filename: String,
    /// The template vars used for this entry's renders, with the per-config
    /// `Amd64` micro-architecture variable scoped in. The write/push path
    /// renders the `directory:` template against these so `{{ .Amd64 }}`
    /// resolves to the same configured variant every other per-entry render
    /// saw — not the stale/empty global value.
    pub(crate) scoped_vars: anodizer_core::template::TemplateVars,
}

/// Derive the pacman `arch=()` list for a source-AUR package from the linux
/// build targets it supports, rather than a hardcoded constant.
///
/// `crate_name` selects per-crate `builds[].targets` when it names a configured
/// crate carrying explicit builds (per-crate config mode); otherwise the
/// workspace-wide configured targets are used (top-level `aur_sources:` and
/// crates that inherit `defaults.targets`). Only linux targets are kept (a
/// source build's `arch=()` advertises the host architectures pacman builds
/// for — darwin/windows triples are not pacman arches), each mapped to its
/// pacman name via [`crate::aur_arch::triple_to_pacman_arch`]. A linux target
/// whose architecture has no pacman name hard-fails rather than being dropped
/// or mislabeled.
///
/// Falls back to `["x86_64"]` only when no linux target is configured at all
/// (a source package must advertise at least one architecture); a degenerate
/// config that builds for no linux target would otherwise emit `arch=()`.
pub(super) fn aur_source_arches(ctx: &Context, crate_name: &str) -> Result<Vec<String>> {
    let default_targets: Vec<String> = ctx.config.effective_default_targets();

    // Per-crate builds take precedence so per-crate config mode resolves its
    // own target set (no cross-crate leakage); otherwise inherit the defaults.
    let crate_cfg = crate::util::find_crate_in_universe(ctx, crate_name);
    let triples: Vec<String> = match crate_cfg.and_then(|c| c.builds.as_deref()) {
        Some(builds) if !builds.is_empty() => {
            let mut seen = std::collections::BTreeSet::new();
            for b in builds {
                let ts = b.targets.as_deref().unwrap_or(&default_targets);
                for t in ts {
                    seen.insert(t.clone());
                }
            }
            seen.into_iter().collect()
        }
        _ => default_targets.clone(),
    };

    let mut arches: Vec<String> = Vec::new();
    for triple in &triples {
        if !anodizer_core::target::is_linux(triple) {
            continue;
        }
        let arch = crate::aur_arch::triple_to_pacman_arch(triple).map_err(|e| {
            anyhow::anyhow!(
                "aur_source: {} (target '{}'). The source PKGBUILD `arch=()` \
                 cannot name this architecture for pacman; emitting it would \
                 advertise an architecture Arch Linux does not build for. \
                 Restrict the build targets to Arch-supported architectures \
                 (x86_64, aarch64, armv7h, i686) or extend the arch mapping.",
                e,
                triple,
            )
        })?;
        if !arches.iter().any(|a| a == arch) {
            arches.push(arch.to_string());
        }
    }

    if arches.is_empty() {
        // No linux target configured — a source package must advertise at
        // least one arch. Default to x86_64 (the universal Arch baseline).
        arches.push("x86_64".to_string());
    }
    Ok(arches)
}

/// Skip-unaware render of a single source-AUR entry's `PKGBUILD` + `.SRCINFO`.
///
/// Resolves every field default, derives the source tarball URL (honoring
/// `url_template`, with the `Amd64` micro-architecture variable scoped onto a
/// throwaway copy of the template vars so this stays a pure read of `ctx`), and
/// renders both artifacts. The skip / `skip_upload` / `if` gate is evaluated by
/// the callers, so this never double-evaluates it and `ctx` is not mutated.
pub(super) fn render_aur_source_inner(
    ctx: &Context,
    cfg: &AurSourceConfig,
    default_name: &str,
    strip_bin_suffix: bool,
    label: &str,
    log: &StageLogger,
) -> Result<AurSourceRender> {
    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_else(|| "0.0.0".to_string())
        .replace('-', "_");

    let raw_name = cfg.name.as_deref().unwrap_or(default_name);
    let pkg_name = if strip_bin_suffix {
        raw_name
            .strip_suffix("-bin")
            .unwrap_or(raw_name)
            .to_string()
    } else {
        raw_name.to_string()
    };

    // Per-crate metadata resolution mirrors the `-bin` AUR publisher
    // (`aur::aur_resolve_fields`): an explicit `aur_source` field wins, else the
    // value resolves through `metadata.*` → the crate's `Cargo.toml [package]`
    // for `default_name`. In workspace per-crate mode `default_name` is the
    // crate name, so each source PKGBUILD carries that crate's real
    // description / homepage / license / maintainers rather than a hardcoded
    // default (which would ship the crate name as description, an empty url, and
    // `MIT` regardless of the crate's true license).
    let description_raw = cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description_for(default_name))
        .unwrap_or(default_name)
        .to_string();
    let homepage_raw = cfg
        .homepage
        .as_deref()
        .or_else(|| ctx.config.meta_homepage_for(default_name))
        .unwrap_or("")
        .to_string();
    // Render the license into the pacman `license=()` array via the shared SPDX
    // parser so a dual-licensed crate (`MIT OR Apache-2.0`) emits
    // `license=('MIT' 'Apache-2.0')` rather than understating to one id. The
    // resolved license (explicit → metadata → crate Cargo.toml) is fed in; an
    // empty result yields an empty `license=()` array, never a `MIT` invention.
    let license_raw = cfg
        .license
        .clone()
        .or_else(|| {
            ctx.config
                .meta_license_for(default_name)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let license: Vec<String> = if license_raw.trim().is_empty() {
        Vec::new()
    } else {
        anodizer_core::license::parse_spdx_expression(&license_raw)
            .ids()
            .to_vec()
    };

    // Derive the pacman `arch=()` set from the linux build targets this source
    // package supports (per-crate when `default_name` names a configured crate
    // with explicit builds; workspace-wide otherwise).
    let arches = aur_source_arches(ctx, default_name)?;

    let pkgrel: u32 = cfg.rel.as_deref().and_then(|r| r.parse().ok()).unwrap_or(1);

    // Surface the configured x86_64 micro-architecture variant as a template
    // var (default `v1`) so user-supplied `prepare` / `build` / `package`
    // scripts and the `url_template` / `directory` templates can branch on the
    // variant when the source builds need to pick CPU-feature-specific cargo
    // flags. Constrained to a typed enum at the config layer (no artifact
    // filter applies — AUR source pkgs build from the upstream tarball, so this
    // is template-only). Scoped onto a clone of the live vars so rendering
    // stays a pure read of `ctx` (the live path and the offline validator both
    // render from the same immutable context); the scoped copy is threaded out
    // so the write/push path's `directory:` render sees the same `Amd64`.
    let amd64_variant = cfg
        .amd64_variant
        .as_ref()
        .map(|v| v.as_str())
        .unwrap_or("v1");
    let mut scoped_vars = ctx.template_vars().clone();
    scoped_vars.set("Amd64", amd64_variant);

    // A user-supplied `description` / `homepage` may carry template syntax
    // (`{{ .Tag }}`); render against the same `Amd64`-scoped vars the
    // `url_template` / `directory` renders use, so the PKGBUILD/.SRCINFO ship
    // the resolved values rather than the literal `{{ … }}` delimiters and the
    // per-config micro-arch variable resolves consistently.
    let is_strict = ctx.render_is_strict();
    let description = util::render_or_warn_with_vars(
        &scoped_vars,
        log,
        "aur_source.description",
        &description_raw,
        is_strict,
    )?;
    let homepage = util::render_or_warn_with_vars(
        &scoped_vars,
        log,
        "aur_source.homepage",
        &homepage_raw,
        is_strict,
    )?;

    // Source URL — use url_template or default release URL
    let tag = ctx.template_vars().get("Tag").cloned().unwrap_or_default();

    let source_url = if let Some(ref tmpl) = cfg.url_template {
        anodizer_core::template::render(tmpl, &scoped_vars)
            .with_context(|| format!("{}: render url_template", label))?
    } else {
        let git_url = ctx
            .template_vars()
            .get("GitURL")
            .cloned()
            .unwrap_or_default();
        let owner = if git_url.contains("://") {
            git_url.split('/').nth(3).unwrap_or("").to_string()
        } else if git_url.contains(':') {
            git_url
                .split(':')
                .nth(1)
                .unwrap_or("")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string()
        } else {
            String::new()
        };
        let project = ctx
            .template_vars()
            .get("ProjectName")
            .cloned()
            .unwrap_or_default();
        if owner.is_empty() {
            log.warn(&format!(
                "could not extract owner from GitURL for {}; set url_template explicitly",
                label
            ));
        }
        format!("https://github.com/{owner}/{project}/archive/refs/tags/{tag}.tar.gz",)
    };

    let maintainers = cfg
        .maintainers
        .clone()
        .unwrap_or_else(|| ctx.config.meta_maintainers_for(default_name).to_vec());
    let contributors = cfg.contributors.clone().unwrap_or_default();
    let depends = cfg.depends.clone().unwrap_or_default();
    let optdepends = cfg.optdepends.clone().unwrap_or_default();
    // An AUR *sources* package is the upstream-build flavor; the canonical
    // conflict is the *upstream* package name (the unsuffixed `<name>`),
    // because `<name>-bin` (the binary AUR variant) exists alongside it.
    // Defaulting to `[<name>-bin]` only conflicts with our own binary AUR
    // package — useless. `[ProjectName]` is the correct default.
    let conflicts = cfg
        .conflicts
        .clone()
        .unwrap_or_else(|| vec![pkg_name.clone()]);
    let provides = cfg
        .provides
        .clone()
        .unwrap_or_else(|| vec![pkg_name.clone()]);
    let backup = cfg.backup.clone().unwrap_or_default();
    let makedepends = cfg
        .makedepends
        .clone()
        .unwrap_or_else(|| vec!["rust".to_string(), "cargo".to_string()]);

    let meta = AurMeta {
        name: &pkg_name,
        version: &version,
        pkgrel,
        description: &description,
        homepage: &homepage,
        license: &license,
        arches: &arches,
    };
    let deps = AurDeps {
        depends: &depends,
        makedepends: &makedepends,
        optdepends: &optdepends,
        conflicts: &conflicts,
        provides: &provides,
    };
    // `.install` file: `<pkgname>.install` is registered when
    // `install:` is set, emitting an `install=<pkgname>.install` line in the
    // PKGBUILD. Mirror the `-bin` AUR publisher: the config value is the file
    // *content*, written alongside PKGBUILD/.SRCINFO.
    let install_filename = format!("{}.install", pkg_name);
    let install_file_ref = cfg.install.as_ref().map(|_| install_filename.as_str());

    let extras = AurExtras {
        people: AurPeople {
            maintainers: &maintainers,
            contributors: &contributors,
        },
        hooks: AurHooks {
            prepare: cfg.prepare.as_deref(),
            build: cfg.build.as_deref(),
            package: cfg.package.as_deref(),
        },
        backup: &backup,
        binary_name: default_name,
        install_file: install_file_ref,
    };
    let pkgbuild = generate_source_pkgbuild(&meta, &deps, &extras, &source_url);
    let srcinfo = generate_source_srcinfo(&meta, &deps, &source_url);
    util::guard_no_unrendered(ctx, log, "aur-source PKGBUILD", &pkgbuild)?;
    util::guard_no_unrendered(ctx, log, "aur-source .SRCINFO", &srcinfo)?;

    Ok(AurSourceRender {
        rendered: AurRendered {
            pkgbuild,
            srcinfo,
            package_name: pkg_name.clone(),
        },
        pkg_name,
        install_filename,
        scoped_vars,
    })
}

/// Render the source-AUR artifacts a live publish would write for a per-crate
/// `aur_source:` block, honoring `skip` / `skip_upload` / the `if:` condition.
///
/// Returns `Ok(None)` when the publisher would skip the crate (a truthy `skip`
/// / `skip_upload` or a falsy `if`), or when the crate carries no `aur_source`
/// block. The live publish path and the offline schema validator both render
/// through the same skip-unaware [`render_aur_source_inner`], so the validated
/// artifacts are byte-for-byte what a release pushes.
pub(crate) fn render_aur_source_pkgbuild_and_srcinfo_for_crate(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<AurRendered>> {
    let Some(cfg) = crate::util::find_crate_in_universe(ctx, crate_name)
        .and_then(|c| c.publish.as_ref())
        .and_then(|p| p.aur_source.as_ref())
        .cloned()
    else {
        return Ok(None);
    };

    let label = format!("aur_source: crate '{crate_name}'");
    if crate::util::should_skip_publisher_with_if(
        ctx,
        cfg.skip.as_ref(),
        cfg.skip_upload.as_ref(),
        cfg.if_condition.as_deref(),
        &label,
        log,
    )? {
        return Ok(None);
    }

    let render = render_aur_source_inner(ctx, &cfg, crate_name, false, "aur_source", log)?;
    Ok(Some(render.rendered))
}

/// Render every applicable top-level `aur_sources:` array entry, honoring each
/// entry's `skip` / `skip_upload` / `if:` gate. Returns an empty Vec when the
/// array is unset/empty or every entry is skipped — the validator treats that
/// as "nothing to validate".
pub(crate) fn render_top_level_aur_source(
    ctx: &Context,
    log: &StageLogger,
) -> Result<Vec<AurRendered>> {
    let entries = match ctx.config.aur_sources {
        Some(ref v) if !v.is_empty() => v.clone(),
        _ => return Ok(Vec::new()),
    };

    let project_name = ctx
        .template_vars()
        .get("ProjectName")
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::new();
    for (i, cfg) in entries.iter().enumerate() {
        let label = format!("aur_sources[{}]", i);
        if crate::util::should_skip_publisher_with_if(
            ctx,
            cfg.skip.as_ref(),
            cfg.skip_upload.as_ref(),
            cfg.if_condition.as_deref(),
            &label,
            log,
        )? {
            continue;
        }
        let render = render_aur_source_inner(ctx, cfg, &project_name, true, &label, log)?;
        out.push(render.rendered);
    }
    Ok(out)
}
