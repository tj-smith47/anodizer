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
    /// saw, instead of the stale or empty global value.
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
/// whose architecture has no pacman name disqualifies the entry (`entry_skip`)
/// rather than being dropped or mislabeled; the caller absorbs the skip and
/// moves on to the next entry.
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
        // The unmappable architecture disqualifies THIS entry only — a hard
        // error aborts the publisher and strands every crate after it.
        let arch = crate::aur_arch::triple_to_pacman_arch(triple).map_err(|e| {
            anodizer_core::pipe_skip::entry_skip(format!(
                "{e} (target '{triple}'). The source PKGBUILD `arch=()` cannot \
                 name this architecture for pacman; emitting it would advertise \
                 an architecture Arch Linux does not build for. Restrict the \
                 build targets to Arch-supported architectures (x86_64, \
                 aarch64, armv7h, i686) or extend the arch mapping."
            ))
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

/// The source archive a source package downloads, resolved from the run.
struct ResolvedSource {
    url: String,
    sha256: String,
    format: String,
    prefix: String,
}

/// Find the source archive the release carries and the URL, checksum,
/// format and top-level directory the PKGBUILD needs from it.
///
/// `url_template` overrides the URL (with `ArtifactName` set to the archive's
/// file name) and is the one way to render without an archive, in which case
/// the checksum is `SKIP`. Without it, a run that produced no source archive
/// disqualifies the entry: the reason names `source.enabled: true` when the
/// stage is off, and the stage not having registered one when it is on
/// (`--skip=source`, or a failure before the archive was registered). An archive whose download URL is not known yet is used
/// under its file name on a snapshot or dry run and disqualifies the entry
/// otherwise.
fn resolve_source_archive(
    ctx: &Context,
    cfg: &AurSourceConfig,
    scoped_vars: &mut anodizer_core::template::TemplateVars,
    label: &str,
) -> Result<ResolvedSource> {
    let archive = ctx
        .artifacts
        .all()
        .iter()
        .find(|a| a.kind == anodizer_core::artifact::ArtifactKind::SourceArchive);
    if let Some(a) = archive {
        scoped_vars.set("ArtifactName", &a.name);
    }

    let url = match (&cfg.url_template, archive) {
        (Some(tmpl), _) => anodizer_core::template::render(tmpl, scoped_vars)
            .with_context(|| format!("{label}: render url_template"))?,
        (None, None) => {
            let reason = if ctx.config.source.as_ref().is_some_and(|s| s.is_enabled()) {
                "no source archive was produced: the source stage did not register one \
                 (`--skip=source`, or it failed before the archive was written)"
            } else {
                "no source archive found: enable `source.enabled: true`"
            };
            return Err(anodizer_core::pipe_skip::entry_skip(reason));
        }
        (None, Some(a)) => match a.metadata.get("url") {
            Some(url) => url.clone(),
            None if ctx.is_snapshot() || ctx.is_dry_run() => a.name.clone(),
            None => {
                return Err(anodizer_core::pipe_skip::entry_skip(format!(
                    "source archive '{}' has no download URL yet; the release \
                     stage sets it when the archive is uploaded",
                    a.name
                )));
            }
        },
    };

    let sha256 = match archive {
        Some(a) => match a.metadata.get("sha256") {
            Some(sum) if !sum.is_empty() => sum.clone(),
            _ if a.path.is_file() => anodizer_core::hashing::sha256_file(&a.path)
                .with_context(|| format!("{label}: checksum {}", a.path.display()))?,
            _ => "SKIP".to_string(),
        },
        None => "SKIP".to_string(),
    };

    let format = archive
        .and_then(|a| a.metadata.get("format").cloned())
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| crate::aur::extract_archive_extension(&url).to_string());

    // The archive's top-level directory travels in its metadata so a
    // `--publish-only` run rehydrated from `artifacts.json` still knows it;
    // the `SourcePrefix` variable covers a run in which the stage just ran.
    // An `artifacts.json` written before the metadata carried `prefix` has
    // neither, so the configured `prefix_template` is rendered the way the
    // source stage renders it.
    let prefix = match archive
        .and_then(|a| a.metadata.get("prefix").cloned())
        .or_else(|| ctx.template_vars().get("SourcePrefix").cloned())
    {
        Some(prefix) => prefix,
        None => match ctx
            .config
            .source
            .as_ref()
            .and_then(|s| s.prefix_template.as_deref())
        {
            Some(tpl) => anodizer_core::config::SourceConfig::archive_top_level_dir(
                &ctx.render_template(tpl)
                    .with_context(|| format!("{label}: render source.prefix_template '{tpl}'"))?,
            ),
            None => String::new(),
        },
    };

    Ok(ResolvedSource {
        url,
        sha256,
        format,
        prefix,
    })
}

/// Skip-unaware render of a single source-AUR entry's `PKGBUILD` + `.SRCINFO`.
///
/// Resolves every field default, resolves the source archive the release
/// carries (honoring `url_template`, with the `Amd64` micro-architecture
/// variable scoped onto a throwaway copy of the template vars so this stays a
/// pure read of `ctx`), and renders both artifacts. The skip / `skip_upload` / `if` gate is evaluated by
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
    let arches = match cfg.arches.as_deref() {
        Some(explicit) if !explicit.is_empty() => {
            let mut arches = explicit.to_vec();
            arches.sort();
            arches.dedup();
            arches
        }
        _ => aur_source_arches(ctx, default_name)?,
    };

    let pkgrel: u32 = cfg.rel.as_deref().and_then(|r| r.parse().ok()).unwrap_or(1);

    // Expose the configured x86_64 micro-architecture variant as a template
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

    let source = resolve_source_archive(ctx, cfg, &mut scoped_vars, label)?;

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
    // Defaulting to `[<name>-bin]` only conflicts with this project's own
    // binary AUR package — useless. `[ProjectName]` is the correct default.
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
    let source_file = AurSourceFile {
        url: &source.url,
        sha256: &source.sha256,
        format: &source.format,
        prefix: &source.prefix,
    };
    let pkgbuild = generate_source_pkgbuild(&meta, &deps, &extras, &source_file);
    let srcinfo = generate_source_srcinfo(&meta, &deps, &extras, &source_file);
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
) -> Result<Vec<(usize, AurRendered)>> {
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
        out.push((i, render.rendered));
    }
    Ok(out)
}
