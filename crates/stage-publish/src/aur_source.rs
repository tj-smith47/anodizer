use anodizer_core::config::AurSourceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::aur::AurRendered;
use crate::util;

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
fn aur_source_arches(ctx: &Context, crate_name: &str) -> Result<Vec<String>> {
    let default_targets: Vec<String> = ctx.config.effective_default_targets();

    // Per-crate builds take precedence so per-crate config mode resolves its
    // own target set (no cross-crate leakage); otherwise inherit the defaults.
    let crate_cfg = ctx.config.crates.iter().find(|c| c.name == crate_name);
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
fn render_aur_source_inner(
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
    let Some(cfg) = ctx
        .config
        .crates
        .iter()
        .find(|c| c.name == crate_name)
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

/// Shared core logic for publishing a single AUR source entry.
///
/// Both per-crate (`publish_to_aur_source`) and top-level
/// (`publish_top_level_aur_sources`) delegate here after resolving which
/// `AurSourceConfig` to use and after evaluating the skip / `if:` gate.
fn publish_aur_source_entry(
    ctx: &mut Context,
    cfg: &AurSourceConfig,
    default_name: &str,
    strip_bin_suffix: bool,
    label: &str,
    log: &StageLogger,
) -> Result<bool> {
    let AurSourceRender {
        rendered: AurRendered {
            pkgbuild, srcinfo, ..
        },
        pkg_name,
        install_filename,
        scoped_vars,
    } = render_aur_source_inner(ctx, cfg, default_name, strip_bin_suffix, label, log)?;

    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_else(|| "0.0.0".to_string())
        .replace('-', "_");

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would publish AUR source package '{}' ({})",
            pkg_name, label
        ));
        log.verbose(&format!("PKGBUILD:\n{}", pkgbuild));
        return Ok(false);
    }

    // Write files to dist
    let dist = ctx.config.dist.clone();
    let aur_dir = dist.join("aur_source").join(&pkg_name);
    std::fs::create_dir_all(&aur_dir)
        .with_context(|| format!("{}: create dir {}", label, aur_dir.display()))?;

    write_aur_source_files(
        &aur_dir,
        &pkgbuild,
        &srcinfo,
        &install_filename,
        cfg.install.as_deref(),
        label,
    )?;

    // Register artifacts
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: anodizer_core::artifact::ArtifactKind::SourcePkgBuild,
        name: "PKGBUILD".to_string(),
        path: aur_dir.join("PKGBUILD"),
        target: None,
        crate_name: pkg_name.clone(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("id".to_string(), pkg_name.clone());
            m.insert("format".to_string(), "aur_source".to_string());
            m
        },
        size: None,
    });

    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: anodizer_core::artifact::ArtifactKind::SourceSrcInfo,
        name: ".SRCINFO".to_string(),
        path: aur_dir.join(".SRCINFO"),
        target: None,
        crate_name: pkg_name.clone(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("id".to_string(), pkg_name.clone());
            m
        },
        size: None,
    });

    // Push to the AUR git repo. An explicit `git_url` is a verbatim
    // override; otherwise derive the canonical AUR remote from the resolved
    // package name (the same `pkgbase` written into PKGBUILD/.SRCINFO) so the
    // push target can never drift from the package name.
    let git_url = aur_source_push_git_url(cfg, &pkg_name);

    let tmp_dir = tempfile::tempdir().context(format!("{}: create temp dir", label))?;
    let repo_path = tmp_dir.path();

    if cfg.private_key.is_some() || cfg.git_ssh_command.is_some() {
        // `private_key` / `git_ssh_command` may be templated
        // (`{{ .Env.AUR_SSH_KEY }}`). Render against the same `Amd64`-scoped
        // vars as the rest of this resource before they reach the SSH clone,
        // or the literal template text is written to the key file and ssh
        // fails with "error in libcrypto".
        let strict = ctx.render_is_strict();
        let rendered_key = match cfg.private_key.as_deref() {
            Some(pk) => Some(util::render_or_warn_with_vars(
                &scoped_vars,
                log,
                "aur_source.private_key",
                pk,
                strict,
            )?),
            None => None,
        };
        let rendered_ssh = match cfg.git_ssh_command.as_deref() {
            Some(sc) => Some(util::render_or_warn_with_vars(
                &scoped_vars,
                log,
                "aur_source.git_ssh_command",
                sc,
                strict,
            )?),
            None => None,
        };
        util::clone_repo_ssh(
            &git_url,
            rendered_key.as_deref(),
            rendered_ssh.as_deref(),
            repo_path,
            label,
            log,
        )?;
    } else {
        util::clone_repo_with_auth(&git_url, None, repo_path, label, log)?;
    }

    let output_dir = if let Some(ref dir) = cfg.directory {
        // Render against the `Amd64`-scoped vars from the inner render so a
        // `directory: "{{ .Amd64 }}/…"` template resolves to the configured
        // variant, consistent with the url_template / hook renders.
        let rendered_dir =
            util::render_or_warn_with_vars(&scoped_vars, log, label, dir, ctx.render_is_strict())?;
        let d = repo_path.join(&rendered_dir);
        std::fs::create_dir_all(&d)?;
        d
    } else {
        repo_path.to_path_buf()
    };

    std::fs::copy(aur_dir.join("PKGBUILD"), output_dir.join("PKGBUILD"))
        .with_context(|| format!("{label}: copy PKGBUILD to output dir"))?;
    std::fs::copy(aur_dir.join(".SRCINFO"), output_dir.join(".SRCINFO"))
        .with_context(|| format!("{label}: copy .SRCINFO to output dir"))?;
    if cfg.install.is_some() {
        std::fs::copy(
            aur_dir.join(&install_filename),
            output_dir.join(&install_filename),
        )
        .with_context(|| format!("{label}: copy {install_filename} to output dir"))?;
    }

    let commit_msg = crate::homebrew::render_commit_msg(
        cfg.commit_msg_template.as_deref(),
        &pkg_name,
        &version,
        "package",
        log,
        ctx.render_is_strict(),
    )?;
    let commit_opts = util::resolve_commit_opts(ctx, cfg.commit_author.as_ref(), log)?;
    let outcome = util::commit_and_push_with_opts(
        repo_path,
        &["."],
        &commit_msg,
        None,
        label,
        &commit_opts,
        log,
    )?;
    match outcome {
        util::CommitOutcome::Pushed => {
            log.status(&format!(
                "pushed package '{}' for {} to {}",
                pkg_name, label, git_url
            ));
        }
        util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "nothing to push for {} — package '{}' already up to date",
                label, pkg_name
            ));
        }
    }
    Ok(outcome.is_pushed())
}

/// Publish AUR source packages for a crate (per-crate config path).
pub fn publish_to_aur_source(
    ctx: &mut Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<bool> {
    let crate_cfg = ctx
        .config
        .crates
        .iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| anyhow::anyhow!("aur_source: crate '{}' not found", crate_name))?;
    let publish_cfg = crate_cfg
        .publish
        .as_ref()
        .and_then(|p| p.aur_source.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "aur_source: no aur_source config for crate '{}'",
                crate_name
            )
        })?
        .clone();

    let label = format!("aur_source: crate '{crate_name}'");
    if crate::util::should_skip_publisher_with_if(
        ctx,
        publish_cfg.skip.as_ref(),
        publish_cfg.skip_upload.as_ref(),
        publish_cfg.if_condition.as_deref(),
        &label,
        log,
    )? {
        return Ok(false);
    }

    publish_aur_source_entry(ctx, &publish_cfg, crate_name, false, "aur_source", log)
}

/// Publish top-level `aur_sources` entries (not tied to a specific crate).
///
/// The AUR-sources publisher reads the `aur_sources` config
/// as a project-wide array. Each entry generates a source PKGBUILD and .SRCINFO,
/// then pushes them to the configured AUR git repo.
pub fn publish_top_level_aur_sources(ctx: &mut Context, log: &StageLogger) -> Result<bool> {
    let entries = match ctx.config.aur_sources {
        Some(ref v) if !v.is_empty() => v.clone(),
        _ => return Ok(false),
    };

    let project_name = ctx
        .template_vars()
        .get("ProjectName")
        .cloned()
        .unwrap_or_default();

    let mut any_pushed = false;
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

        any_pushed |= publish_aur_source_entry(ctx, cfg, &project_name, true, &label, log)?;
    }

    Ok(any_pushed)
}

// ---------------------------------------------------------------------------
// AUR source render specs
// ---------------------------------------------------------------------------
//
// The original `generate_source_srcinfo` and `generate_source_pkgbuild`
// functions took 12 and 19 positional arguments respectively. Bundle them
// so each public entry point lands well under clippy's threshold and so
// fields that are truly identical between the two render paths
// (`AurMeta`, `AurDeps`) are guaranteed to stay in lock-step.

/// Package identity — name, version, pkgrel, description, homepage, license.
/// Shared by [`generate_source_srcinfo`] and [`generate_source_pkgbuild`].
#[derive(Clone, Copy)]
struct AurMeta<'a> {
    name: &'a str,
    version: &'a str,
    pkgrel: u32,
    description: &'a str,
    homepage: &'a str,
    /// Rendered pacman `license=()` entries (SPDX-id-split for dual-licensed
    /// crates). Empty when no license configured.
    license: &'a [String],
    /// Pacman `arch=()` entries derived from the linux build targets the
    /// source package supports (not a hardcoded constant).
    arches: &'a [String],
}

/// Dependency lists — the five `depends`/`makedepends`/`optdepends`/
/// `conflicts`/`provides` arrays. Shared by both renderers.
#[derive(Clone, Copy)]
struct AurDeps<'a> {
    depends: &'a [String],
    makedepends: &'a [String],
    optdepends: &'a [String],
    conflicts: &'a [String],
    provides: &'a [String],
}

/// People credits — `# Maintainer:` / `# Contributor:` comment lines.
/// PKGBUILD-only (.SRCINFO does not surface these).
#[derive(Clone, Copy)]
struct AurPeople<'a> {
    maintainers: &'a [String],
    contributors: &'a [String],
}

/// User-supplied PKGBUILD function bodies. Each is opt-in; when `None`,
/// the renderer emits the default cargo-based body.
#[derive(Clone, Copy)]
struct AurHooks<'a> {
    prepare: Option<&'a str>,
    build: Option<&'a str>,
    package: Option<&'a str>,
}

/// Everything PKGBUILD-only beyond `meta` + `deps` + `source_url`.
/// Bundles people, hooks, backup file list, and the binary name used by
/// the default build/package bodies.
#[derive(Clone, Copy)]
struct AurExtras<'a> {
    people: AurPeople<'a>,
    hooks: AurHooks<'a>,
    backup: &'a [String],
    binary_name: &'a str,
    /// When set, the PKGBUILD emits `install=<name>.install` and the
    /// `.install` file (post-install/pre-remove scripts) is written
    /// alongside it (the `install:` field).
    install_file: Option<&'a str>,
}

/// Write `PKGBUILD`, `.SRCINFO`, and the optional `.install` file into
/// `aur_dir`. The `.install` file (`<install_filename>`) is only written when
/// `install_content` is `Some`, mirroring the `-bin` AUR publisher's
/// `aur_write_package_files`.
fn write_aur_source_files(
    aur_dir: &std::path::Path,
    pkgbuild: &str,
    srcinfo: &str,
    install_filename: &str,
    install_content: Option<&str>,
    label: &str,
) -> Result<()> {
    std::fs::write(aur_dir.join("PKGBUILD"), pkgbuild)
        .with_context(|| format!("{}: write PKGBUILD", label))?;
    std::fs::write(aur_dir.join(".SRCINFO"), srcinfo)
        .with_context(|| format!("{}: write .SRCINFO", label))?;
    if let Some(content) = install_content {
        std::fs::write(aur_dir.join(install_filename), content)
            .with_context(|| format!("{}: write {}", label, install_filename))?;
    }
    Ok(())
}

/// Generate a .SRCINFO file for a source AUR package.
fn generate_source_srcinfo(meta: &AurMeta<'_>, deps: &AurDeps<'_>, source_url: &str) -> String {
    let AurMeta {
        name,
        version,
        pkgrel,
        description,
        homepage,
        license,
        arches,
    } = *meta;
    let AurDeps {
        depends,
        makedepends,
        optdepends,
        conflicts,
        provides,
    } = *deps;

    let mut lines = Vec::new();
    lines.push(format!("pkgbase = {}", name));
    lines.push(format!("\tpkgdesc = {}", description));
    lines.push(format!("\tpkgver = {}", version));
    lines.push(format!("\tpkgrel = {}", pkgrel));
    if !homepage.is_empty() {
        lines.push(format!("\turl = {}", homepage));
    }
    for a in arches {
        lines.push(format!("\tarch = {}", a));
    }
    for l in license {
        lines.push(format!("\tlicense = {}", l));
    }
    for d in makedepends {
        lines.push(format!("\tmakedepends = {}", d));
    }
    for d in depends {
        lines.push(format!("\tdepends = {}", d));
    }
    for d in optdepends {
        lines.push(format!("\toptdepends = {}", d));
    }
    for c in conflicts {
        lines.push(format!("\tconflicts = {}", c));
    }
    for p in provides {
        lines.push(format!("\tprovides = {}", p));
    }
    lines.push(format!("\tsource = {}", source_url));
    lines.push("\tsha256sums = SKIP".to_string());
    lines.push(String::new());
    lines.push(format!("pkgname = {}", name));
    lines.join("\n")
}

/// Generate a source-only PKGBUILD that builds from source using cargo.
fn generate_source_pkgbuild(
    meta: &AurMeta<'_>,
    deps: &AurDeps<'_>,
    extras: &AurExtras<'_>,
    source_url: &str,
) -> String {
    let AurMeta {
        name,
        version,
        pkgrel,
        description,
        homepage,
        license,
        arches,
    } = *meta;
    let AurDeps {
        depends,
        makedepends,
        optdepends,
        conflicts,
        provides,
    } = *deps;
    let AurExtras {
        people: AurPeople {
            maintainers,
            contributors,
        },
        hooks: AurHooks {
            prepare,
            build,
            package,
        },
        backup,
        binary_name,
        install_file,
    } = *extras;

    let mut lines = Vec::new();

    // Header comments
    for m in maintainers {
        lines.push(format!("# Maintainer: {}", m));
    }
    for c in contributors {
        lines.push(format!("# Contributor: {}", c));
    }
    if !maintainers.is_empty() || !contributors.is_empty() {
        lines.push(String::new());
    }

    lines.push(format!("pkgname='{}'", name));
    lines.push(format!("pkgver='{}'", version));
    lines.push(format!("pkgrel={}", pkgrel));
    lines.push(format!("pkgdesc=\"{}\"", description));
    let arch_entries: Vec<String> = arches.iter().map(|a| format!("'{}'", a)).collect();
    lines.push(format!("arch=({})", arch_entries.join(" ")));
    if !homepage.is_empty() {
        lines.push(format!("url='{}'", homepage));
    }
    let license_entries: Vec<String> = license.iter().map(|l| format!("'{}'", l)).collect();
    lines.push(format!("license=({})", license_entries.join(" ")));

    if !depends.is_empty() {
        let d: Vec<String> = depends.iter().map(|s| format!("'{}'", s)).collect();
        lines.push(format!("depends=({})", d.join(" ")));
    }
    if !makedepends.is_empty() {
        let d: Vec<String> = makedepends.iter().map(|s| format!("'{}'", s)).collect();
        lines.push(format!("makedepends=({})", d.join(" ")));
    }
    if !optdepends.is_empty() {
        let d: Vec<String> = optdepends.iter().map(|s| format!("'{}'", s)).collect();
        lines.push(format!("optdepends=({})", d.join(" ")));
    }
    if !conflicts.is_empty() {
        let d: Vec<String> = conflicts.iter().map(|s| format!("'{}'", s)).collect();
        lines.push(format!("conflicts=({})", d.join(" ")));
    }
    if !provides.is_empty() {
        let d: Vec<String> = provides.iter().map(|s| format!("'{}'", s)).collect();
        lines.push(format!("provides=({})", d.join(" ")));
    }
    if !backup.is_empty() {
        let d: Vec<String> = backup.iter().map(|s| format!("'{}'", s)).collect();
        lines.push(format!("backup=({})", d.join(" ")));
    }

    if let Some(install_file) = install_file {
        lines.push(format!("install={}", install_file));
    }

    lines.push(format!("source=(\"{}\")", source_url));
    lines.push("sha256sums=('SKIP')".to_string());

    lines.push(String::new());

    // prepare() function
    if let Some(prep) = prepare {
        lines.push("prepare() {".to_string());
        for line in prep.lines() {
            lines.push(format!("  {}", line));
        }
        lines.push("}".to_string());
        lines.push(String::new());
    }

    // build() function
    lines.push("build() {".to_string());
    if let Some(b) = build {
        for line in b.lines() {
            lines.push(format!("  {}", line));
        }
    } else {
        lines.push(format!("  cd \"$srcdir/{}-$pkgver\"", binary_name));
        lines.push("  cargo build --release --locked".to_string());
    }
    lines.push("}".to_string());
    lines.push(String::new());

    // package() function
    lines.push("package() {".to_string());
    if let Some(pkg) = package {
        for line in pkg.lines() {
            lines.push(format!("  {}", line));
        }
    } else {
        lines.push(format!("  cd \"$srcdir/{}-$pkgver\"", binary_name));
        lines.push(format!(
            "  install -Dm755 \"target/release/{}\" \"$pkgdir/usr/bin/{}\"",
            binary_name, binary_name
        ));
        // LICENSE — REQUIRED for non-common licenses; install any LICENSE* the
        // upstream source tree carries. Existence-gated so a tree without one
        // does not fail the build.
        lines.push(
            "  for _l in LICENSE*; do [ -e \"$_l\" ] && \
             install -Dm644 \"$_l\" \"$pkgdir/usr/share/licenses/$pkgname/$_l\"; done"
                .to_string(),
        );
    }
    lines.push("}".to_string());

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// AurSourcePublisher — Publisher trait wrapper (Submitter group)
// ---------------------------------------------------------------------------
//
// Submitter-group; upstream-AUR force-push publisher. Distinct from
// [`crate::aur::AurOurPublisher`] in `aur.rs` which is Manager group with
// `git revert`-based rollback against AUR repos we own. This publisher
// covers the **upstream-AUR source-package** flow: it generates a
// PKGBUILD/.SRCINFO and force-pushes them to an AUR git repo
// (`ssh://aur@aur.archlinux.org/<package>.git`). The push is irreversible
// without coordinating with the AUR maintainer, so rollback is
// warn-only.
//
// CREDENTIAL HANDLING: [`AurSourceTarget`] stores no key material. The
// SSH private key / `GIT_SSH_COMMAND` resolved at publish time
// (`cfg.private_key`, `cfg.git_ssh_command`) is irrelevant to a
// warn-only rollback. We only name the env-var scope operators are
// expected to control (`AUR_SSH_KEY write`) — never the resolved
// secret.

// Submitter-group `Publisher` for the upstream-AUR force-push
// source-publishing flow. Wraps both `publish_to_aur_source` (per-crate)
// and `publish_top_level_aur_sources` (top-level `aur_sources:` array).
//
// Disambiguation: this publisher is NOT the same as
// `crate::aur::AurOurPublisher`. That one is Manager group, with a
// `git revert`-based rollback against AUR repos we own. This one is
// Submitter group, force-pushes upstream AUR repos, and has no
// programmatic rollback.
simple_publisher!(
    AurSourcePublisher,
    "upstream-aur",
    anodizer_core::PublisherGroup::Submitter,
    false,
    Some("AUR_SSH_KEY write"),
);

/// Serialized shape of a recorded upstream-AUR force-push target.
///
/// `package` is the resolved AUR package name (post-template, post
/// `-bin` strip when relevant); `tag` is the current
/// [`anodizer_core::context::Context::version`] tag the source archive
/// references. `git_url` is the `ssh://aur@aur.archlinux.org/...`
/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// (`private_key` / `git_ssh_command`) have no slot to land in. See
/// the Submitter rustdoc above for the credential-handling rationale.
type AurSourceTarget = anodizer_core::publish_evidence::AurSourceTargetSnapshot;

/// Decode the `aur_source_targets` array from
/// [`anodizer_core::PublishEvidence::extra`].
fn decode_aur_source_targets(extra: &anodizer_core::PublishEvidenceExtra) -> Vec<AurSourceTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::AurSource(a) => a.aur_source_targets.clone(),
        _ => Vec::new(),
    }
}

/// True when at least one crate has a `publish.aur_source` block OR the
/// top-level `aur_sources:` array is non-empty. Mirrors the dispatch in
/// `lib.rs` so the publisher runs whenever the existing per-crate +
/// top-level macros would have.
pub(crate) fn is_aur_source_configured(ctx: &Context) -> bool {
    let per_crate = ctx
        .config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.aur_source.is_some()));
    let top_level = ctx
        .config
        .aur_sources
        .as_ref()
        .is_some_and(|v| !v.is_empty());
    per_crate || top_level
}

/// True when the named crate has a `publish.aur_source` block. Per-crate
/// gate for the iteration in `run()`.
pub(crate) fn is_aur_source_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::util::all_crates(ctx)
        .into_iter()
        .any(|c| c.name == crate_name && c.publish.as_ref().is_some_and(|p| p.aur_source.is_some()))
}

/// Reproduce the AUR-source package-name resolution that
/// `publish_aur_source_entry` uses: explicit `cfg.name` wins, otherwise
/// the default name (crate name for per-crate, project name for top-level)
/// with optional `-bin` stripping.
fn resolve_aur_source_package_name(
    cfg: &anodizer_core::config::AurSourceConfig,
    default_name: &str,
    strip_bin_suffix: bool,
) -> String {
    let raw = cfg.name.as_deref().unwrap_or(default_name);
    if strip_bin_suffix {
        raw.strip_suffix("-bin").unwrap_or(raw).to_string()
    } else {
        raw.to_string()
    }
}

/// Resolve the AUR push remote for a source package: an explicit
/// `cfg.git_url` is a verbatim override; otherwise derive the canonical
/// `ssh://aur@aur.archlinux.org/<pkg_name>.git` from the resolved package
/// name, so the push target tracks `pkgbase` and cannot drift.
fn aur_source_push_git_url(cfg: &AurSourceConfig, pkg_name: &str) -> String {
    cfg.git_url
        .as_deref()
        .filter(|u| !u.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| util::aur_default_git_url(pkg_name))
}

/// Build an [`AurSourceTarget`] for a single per-crate `aur_source:` block.
fn collect_aur_source_per_crate_target(ctx: &Context, crate_name: &str) -> Option<AurSourceTarget> {
    let c = ctx.config.crates.iter().find(|c| c.name == crate_name)?;
    let cfg = c.publish.as_ref().and_then(|p| p.aur_source.as_ref())?;
    let pkg_name = resolve_aur_source_package_name(cfg, crate_name, false);
    let git_url = aur_source_push_git_url(cfg, &pkg_name);
    Some(AurSourceTarget {
        target: format!("aur_source: crate '{}'", crate_name),
        package: pkg_name,
        tag: ctx.version(),
        git_url,
    })
}

/// Build [`AurSourceTarget`]s for every entry in the top-level
/// `aur_sources:` array.
fn collect_aur_source_top_level_targets(ctx: &Context) -> Vec<AurSourceTarget> {
    let mut out: Vec<AurSourceTarget> = Vec::new();
    let Some(entries) = ctx.config.aur_sources.as_ref() else {
        return out;
    };
    let project_name = ctx
        .template_vars()
        .get("ProjectName")
        .cloned()
        .unwrap_or_default();
    for (i, cfg) in entries.iter().enumerate() {
        let pkg_name = resolve_aur_source_package_name(cfg, &project_name, true);
        let git_url = aur_source_push_git_url(cfg, &pkg_name);
        out.push(AurSourceTarget {
            target: format!("aur_sources[{}]", i),
            package: pkg_name,
            tag: ctx.version(),
            git_url,
        });
    }
    out
}

impl anodizer_core::Publisher for AurSourcePublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }
    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }
    fn required(&self) -> bool {
        Self::resolved_required(self)
    }
    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }
    fn skips_on_nightly(&self) -> bool {
        true
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // Both config homes: per-crate `publish.aur_source` and the
        // top-level `aur_sources:` block (the same union
        // `is_aur_source_configured` gates dispatch on).
        let per_crate = anodizer_core::env_preflight::crate_universe(&ctx.config)
            .into_iter()
            .filter_map(|c| c.publish.as_ref()?.aur_source.as_ref())
            .filter(|a| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    a.skip.as_ref(),
                    a.skip_upload.as_ref(),
                    a.if_condition.as_deref(),
                )
            })
            .flat_map(|a| {
                crate::publisher_helpers::aur_ssh_requirements(
                    a.private_key.as_deref(),
                    a.git_ssh_command.as_deref(),
                )
            });
        let top_level = ctx
            .config
            .aur_sources
            .iter()
            .flatten()
            .filter(|a| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    a.skip.as_ref(),
                    a.skip_upload.as_ref(),
                    a.if_condition.as_deref(),
                )
            })
            .flat_map(|a| {
                crate::publisher_helpers::aur_ssh_requirements(
                    a.private_key.as_deref(),
                    a.git_ssh_command.as_deref(),
                )
            });
        per_crate.chain(top_level).collect()
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let mut targets: Vec<AurSourceTarget> = Vec::new();
        let mut any_pushed = false;
        // Implicit-all: when --crate is not passed, walk every crate with a
        // `publish.aur_source` block. Reading `selected_crates` raw here
        // would silently skip per-crate configs — see
        // [`crate::publisher_helpers::effective_publish_crates`].
        let selected = crate::publisher_helpers::effective_publish_crates(
            ctx,
            is_aur_source_per_crate_configured,
        );
        // Per-crate aur_source blocks.
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has
            // no aur_source block; implicit-all is already filtered above.
            if !is_aur_source_per_crate_configured(ctx, crate_name) {
                continue;
            }
            // Re-scope the version/name template vars to THIS crate's own tag so
            // the rendered PKGBUILD `pkgver` — AND the recorded source tag —
            // carry the crate's version, not the first crate's (workspace
            // per-crate independent-version mode). The target snapshot is taken
            // inside the same scope so its recorded `tag` matches what is pushed.
            let (pushed, target) = crate::publisher_helpers::with_published_crate_scope(
                ctx,
                crate_name,
                &anodizer_core::crate_scope::resolve_crate_tag,
                |ctx| {
                    let target = collect_aur_source_per_crate_target(ctx, crate_name);
                    let pushed = publish_to_aur_source(ctx, crate_name, &log)?;
                    Ok((pushed, target))
                },
            )?;
            any_pushed |= pushed;
            if let Some(t) = target {
                targets.push(t);
            }
        }
        // Top-level aur_sources array (project-wide).
        let top_level_targets = collect_aur_source_top_level_targets(ctx);
        if !top_level_targets.is_empty() {
            targets.extend(top_level_targets);
            any_pushed |= publish_top_level_aur_sources(ctx, &log)?;
        }
        if !any_pushed {
            targets.clear();
        }
        let mut evidence = anodizer_core::PublishEvidence::new("upstream-aur");
        if let Some(first) = targets.first() {
            evidence.primary_ref = Some(format!(
                "https://aur.archlinux.org/packages/{}",
                first.package
            ));
        }
        evidence.extra = anodizer_core::PublishEvidenceExtra::AurSource(
            anodizer_core::publish_evidence::AurSourceExtra {
                aur_source_targets: targets,
            },
        );
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_aur_source_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "upstream-aur",
                "recorded force-pushes",
            ));
            return Ok(());
        }
        for t in &targets {
            log.warn(&format!(
                "upstream-aur force-push to '{}' at tag '{}' is irreversible \
                 without AUR maintainer coordination; verify state at \
                 https://aur.archlinux.org/packages/{} (git URL: {})",
                t.package, t.tag, t.package, t.git_url
            ));
        }
        log.status(&format!(
            "upstream-aur recorded {} force-push(es); irreversible",
            targets.len()
        ));
        Ok(())
    }

    /// Probe AUR maintainer-key reachability before any publisher runs. This
    /// publisher has no companion state-query checker and force-pushes (the
    /// destructive variant), so an unauthorized key is worth surfacing early —
    /// but the SSH handshake is flaky, so a failure warns rather than blocks.
    fn preflight(&self, ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        let per_crate = anodizer_core::env_preflight::crate_universe(&ctx.config)
            .into_iter()
            .filter_map(|c| c.publish.as_ref()?.aur_source.as_ref())
            .filter(|a| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    a.skip.as_ref(),
                    a.skip_upload.as_ref(),
                    a.if_condition.as_deref(),
                )
            })
            .map(|a| (a.private_key.as_deref(), a.git_ssh_command.as_deref()));
        let top_level = ctx
            .config
            .aur_sources
            .iter()
            .flatten()
            .filter(|a| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    a.skip.as_ref(),
                    a.skip_upload.as_ref(),
                    a.if_condition.as_deref(),
                )
            })
            .map(|a| (a.private_key.as_deref(), a.git_ssh_command.as_deref()));
        let entries: Vec<_> = per_crate.chain(top_level).collect();
        Ok(crate::aur::aur_ssh_auth_preflight(
            ctx,
            entries,
            "upstream-aur",
        ))
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::config::{AurSourceConfig, CrateConfig, PublishConfig};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    fn aur_source_crate(name: &str, git_url: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                aur_source: Some(AurSourceConfig {
                    git_url: Some(git_url.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn aur_source_publisher_classification() {
        let p = AurSourcePublisher::new();
        assert_eq!(p.name(), "upstream-aur");
        assert_eq!(p.group(), PublisherGroup::Submitter);
        assert!(!p.required());
        assert_eq!(p.rollback_scope_needed(), Some("AUR_SSH_KEY write"));
    }

    /// `git_url` unset → derives `ssh://aur@aur.archlinux.org/<pkg>.git`
    /// (no `-bin` suffix for source packages); an explicit `git_url` is used
    /// verbatim; an empty-string `git_url` is treated as unset.
    #[test]
    fn aur_source_push_git_url_derives_from_name() {
        use anodizer_core::config::AurSourceConfig;

        // Per-crate path: default name is the crate name (no -bin strip).
        let cfg = AurSourceConfig::default();
        let pkg = resolve_aur_source_package_name(&cfg, "mytool", false);
        assert_eq!(pkg, "mytool");
        assert_eq!(
            aur_source_push_git_url(&cfg, &pkg),
            "ssh://aur@aur.archlinux.org/mytool.git",
        );

        // Top-level path: default name is the project name with a trailing
        // `-bin` stripped, so a `foo-bin` project yields `foo`.
        let pkg_top = resolve_aur_source_package_name(&cfg, "foo-bin", true);
        assert_eq!(pkg_top, "foo");
        assert_eq!(
            aur_source_push_git_url(&cfg, &pkg_top),
            "ssh://aur@aur.archlinux.org/foo.git",
        );

        // Explicit `name:` override → url tracks the override.
        let cfg_name = AurSourceConfig {
            name: Some("widget".to_string()),
            ..Default::default()
        };
        let pkg_name = resolve_aur_source_package_name(&cfg_name, "mytool", false);
        assert_eq!(
            aur_source_push_git_url(&cfg_name, &pkg_name),
            "ssh://aur@aur.archlinux.org/widget.git",
        );

        // Empty-string git_url is treated as unset (still derives).
        let cfg_empty = AurSourceConfig {
            git_url: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(
            aur_source_push_git_url(&cfg_empty, "mytool"),
            "ssh://aur@aur.archlinux.org/mytool.git",
        );

        // Explicit git_url is a verbatim override.
        let cfg_override = AurSourceConfig {
            git_url: Some("ssh://aur@aur.archlinux.org/custom.git".to_string()),
            name: Some("widget".to_string()),
            ..Default::default()
        };
        assert_eq!(
            aur_source_push_git_url(&cfg_override, "widget"),
            "ssh://aur@aur.archlinux.org/custom.git",
        );
    }

    #[test]
    fn aur_source_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = AurSourcePublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn aur_source_rollback_warns_when_no_targets_recorded() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("upstream-aur");
        let p = AurSourcePublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("upstream-aur")
                && m.contains("recorded force-pushes")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }

    #[test]
    fn aur_source_rollback_warns_per_target_when_evidence_present() {
        let mut ctx = TestContextBuilder::new().build();
        let mut evidence = PublishEvidence::new("upstream-aur");
        evidence.extra = anodizer_core::PublishEvidenceExtra::AurSource(
            anodizer_core::publish_evidence::AurSourceExtra {
                aur_source_targets: vec![
                    AurSourceTarget {
                        target: "aur_source: crate 'demo'".into(),
                        package: "demo".into(),
                        tag: "1.2.3".into(),
                        git_url: "ssh://aur@aur.archlinux.org/demo.git".into(),
                    },
                    AurSourceTarget {
                        target: "aur_sources[0]".into(),
                        package: "widget".into(),
                        tag: "1.2.3".into(),
                        git_url: "ssh://aur@aur.archlinux.org/widget.git".into(),
                    },
                ],
            },
        );
        let p = AurSourcePublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());
        assert_eq!(decode_aur_source_targets(&evidence.extra).len(), 2);
    }

    #[test]
    fn aur_source_target_extra_roundtrips() {
        let original = vec![AurSourceTarget {
            target: "aur_source: crate 'demo'".into(),
            package: "demo".into(),
            tag: "1.2.3".into(),
            git_url: "ssh://aur@aur.archlinux.org/demo.git".into(),
        }];
        let extra = anodizer_core::PublishEvidenceExtra::AurSource(
            anodizer_core::publish_evidence::AurSourceExtra {
                aur_source_targets: original.clone(),
            },
        );
        let decoded = decode_aur_source_targets(&extra);
        assert_eq!(decoded, original);
    }

    #[test]
    fn aur_source_target_extra_carries_no_secret_material() {
        // Structural pin: build a typed-variant evidence and assert
        // (a) no credential-shaped keys appear AND (b) the
        // operator-public coordinates are preserved. The
        // `AurSourceTargetSnapshot` type has no field for
        // `private_key` / `git_ssh_command`, so the type system
        // rejects any future leak attempt at the encode boundary.
        let mut e = PublishEvidence::new("upstream-aur");
        e.extra = anodizer_core::PublishEvidenceExtra::AurSource(
            anodizer_core::publish_evidence::AurSourceExtra {
                aur_source_targets: vec![AurSourceTarget {
                    target: "aur_source: crate 'demo'".into(),
                    package: "demo".into(),
                    tag: "1.2.3".into(),
                    git_url: "ssh://aur@aur.archlinux.org/demo.git".into(),
                }],
            },
        );
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(!s.contains("\"private_key\":"), "{s}");
        assert!(!s.contains("\"git_ssh_command\":"), "{s}");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"auth\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"secret\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        // Positive shape: operator-public coordinates serialize.
        assert!(s.contains("\"package\":\"demo\""), "{s}");
        assert!(s.contains("\"tag\":\"1.2.3\""), "{s}");
        assert!(
            s.contains("\"git_url\":\"ssh://aur@aur.archlinux.org/demo.git\""),
            "{s}"
        );
    }

    #[test]
    fn aur_source_collect_per_crate_target_uses_default_name() {
        let ctx = TestContextBuilder::new()
            .crates(vec![aur_source_crate(
                "demo",
                "ssh://aur@aur.archlinux.org/demo.git",
            )])
            .build();
        let t = collect_aur_source_per_crate_target(&ctx, "demo").expect("target");
        assert_eq!(t.package, "demo");
        assert_eq!(t.git_url, "ssh://aur@aur.archlinux.org/demo.git");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_source_pkgbuild() {
        let maintainers = vec!["Test <test@example.com>".to_string()];
        let depends = vec!["openssl".to_string()];
        let makedepends = vec!["rust".to_string(), "cargo".to_string()];
        let conflicts = vec!["myapp-bin".to_string()];
        let provides = vec!["myapp".to_string()];
        let meta = AurMeta {
            name: "myapp",
            version: "1.0.0",
            pkgrel: 1,
            description: "A test application",
            homepage: "https://example.com",
            license: &["MIT".to_string()],
            arches: &["x86_64".to_string(), "aarch64".to_string()],
        };
        let deps = AurDeps {
            depends: &depends,
            makedepends: &makedepends,
            optdepends: &[],
            conflicts: &conflicts,
            provides: &provides,
        };
        let extras = AurExtras {
            people: AurPeople {
                maintainers: &maintainers,
                contributors: &[],
            },
            hooks: AurHooks {
                prepare: None,
                build: None,
                package: None,
            },
            backup: &[],
            binary_name: "myapp",
            install_file: None,
        };
        let pkgbuild = generate_source_pkgbuild(
            &meta,
            &deps,
            &extras,
            "https://github.com/user/myapp/archive/refs/tags/v1.0.0.tar.gz",
        );

        assert!(pkgbuild.contains("pkgname='myapp'"));
        assert!(pkgbuild.contains("pkgver='1.0.0'"));
        assert!(pkgbuild.contains("pkgrel=1"));
        assert!(pkgbuild.contains("arch=('x86_64' 'aarch64')"));
        assert!(pkgbuild.contains("makedepends=('rust' 'cargo')"));
        assert!(pkgbuild.contains("conflicts=('myapp-bin')"));
        assert!(pkgbuild.contains("cargo build --release --locked"));
        assert!(pkgbuild.contains("install -Dm755"));
        assert!(pkgbuild.contains("# Maintainer: Test <test@example.com>"));
    }

    #[test]
    fn test_generate_source_pkgbuild_custom_build() {
        let meta = AurMeta {
            name: "myapp",
            version: "1.0.0",
            pkgrel: 1,
            description: "Test",
            homepage: "",
            license: &["MIT".to_string()],
            arches: &["x86_64".to_string(), "aarch64".to_string()],
        };
        let deps = AurDeps {
            depends: &[],
            makedepends: &[],
            optdepends: &[],
            conflicts: &[],
            provides: &[],
        };
        let extras = AurExtras {
            people: AurPeople {
                maintainers: &[],
                contributors: &[],
            },
            hooks: AurHooks {
                prepare: Some("cd myapp\npatch -p1 < fix.patch"),
                build: Some("make"),
                package: Some("make install DESTDIR=\"$pkgdir\""),
            },
            backup: &[],
            binary_name: "myapp",
            install_file: None,
        };
        let pkgbuild =
            generate_source_pkgbuild(&meta, &deps, &extras, "https://example.com/source.tar.gz");

        assert!(pkgbuild.contains("prepare() {"));
        assert!(pkgbuild.contains("patch -p1 < fix.patch"));
        assert!(pkgbuild.contains("make\n}"));
        assert!(pkgbuild.contains("make install DESTDIR=\"$pkgdir\""));
    }

    #[test]
    fn test_generate_source_pkgbuild_install_file() {
        let meta = AurMeta {
            name: "myapp",
            version: "1.0.0",
            pkgrel: 1,
            description: "Test",
            homepage: "",
            license: &["MIT".to_string()],
            arches: &["x86_64".to_string(), "aarch64".to_string()],
        };
        let deps = AurDeps {
            depends: &[],
            makedepends: &[],
            optdepends: &[],
            conflicts: &[],
            provides: &[],
        };
        // install=<name>.install only emitted when install_file is Some.
        let with = AurExtras {
            people: AurPeople {
                maintainers: &[],
                contributors: &[],
            },
            hooks: AurHooks {
                prepare: None,
                build: None,
                package: None,
            },
            backup: &[],
            binary_name: "myapp",
            install_file: Some("myapp.install"),
        };
        let pkgbuild =
            generate_source_pkgbuild(&meta, &deps, &with, "https://example.com/source.tar.gz");
        assert!(
            pkgbuild.contains("install=myapp.install"),
            "PKGBUILD must emit install=<name>.install when set:\n{pkgbuild}"
        );

        let without = AurExtras {
            install_file: None,
            ..with
        };
        let pkgbuild_none =
            generate_source_pkgbuild(&meta, &deps, &without, "https://example.com/source.tar.gz");
        assert!(
            !pkgbuild_none.contains("install="),
            "PKGBUILD must NOT emit install= when unset:\n{pkgbuild_none}"
        );
    }

    #[test]
    fn test_write_aur_source_files_writes_install() {
        let dir = tempfile::tempdir().unwrap();
        // With install content: the .install file is written.
        write_aur_source_files(
            dir.path(),
            "PKGBUILD-body",
            "SRCINFO-body",
            "myapp.install",
            Some("post_install() { echo hi; }"),
            "aur_source",
        )
        .unwrap();
        assert!(dir.path().join("PKGBUILD").exists());
        assert!(dir.path().join(".SRCINFO").exists());
        let install_path = dir.path().join("myapp.install");
        assert!(
            install_path.exists(),
            ".install file must be written when content is set"
        );
        assert_eq!(
            std::fs::read_to_string(&install_path).unwrap(),
            "post_install() { echo hi; }"
        );

        // Without install content: no .install file appears.
        let dir2 = tempfile::tempdir().unwrap();
        write_aur_source_files(
            dir2.path(),
            "PKGBUILD-body",
            "SRCINFO-body",
            "myapp.install",
            None,
            "aur_source",
        )
        .unwrap();
        assert!(
            !dir2.path().join("myapp.install").exists(),
            ".install file must NOT be written when content is unset"
        );
    }

    #[test]
    fn test_generate_source_srcinfo() {
        let depends = vec!["openssl".to_string()];
        let makedepends = vec!["rust".to_string(), "cargo".to_string()];
        let conflicts = vec!["myapp-bin".to_string()];
        let provides = vec!["myapp".to_string()];
        let meta = AurMeta {
            name: "myapp",
            version: "1.0.0",
            pkgrel: 1,
            description: "A test application",
            homepage: "https://example.com",
            license: &["MIT".to_string()],
            arches: &["x86_64".to_string(), "aarch64".to_string()],
        };
        let deps = AurDeps {
            depends: &depends,
            makedepends: &makedepends,
            optdepends: &[],
            conflicts: &conflicts,
            provides: &provides,
        };
        let srcinfo = generate_source_srcinfo(
            &meta,
            &deps,
            "https://github.com/user/myapp/archive/refs/tags/v1.0.0.tar.gz",
        );

        assert!(srcinfo.contains("pkgbase = myapp"));
        assert!(srcinfo.contains("\tpkgver = 1.0.0"));
        assert!(srcinfo.contains("\tmakedepends = rust"));
        assert!(srcinfo.contains("\tdepends = openssl"));
        assert!(srcinfo.contains("\tconflicts = myapp-bin"));
        assert!(srcinfo.contains("\tprovides = myapp"));
        assert!(srcinfo.contains("pkgname = myapp"));
    }

    #[test]
    fn test_top_level_aur_sources_config_parsing() {
        use anodizer_core::config::Config;

        let yaml = r#"
project_name: test
aur_sources:
  - name: myapp
    description: "My application"
    license: MIT
    makedepends:
      - rust
      - cargo
    git_url: "ssh://aur@aur.archlinux.org/myapp.git"
  - name: myapp-extra
    description: "Extra package"
    license: MIT
    git_url: "ssh://aur@aur.archlinux.org/myapp-extra.git"
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let aur_sources = config.aur_sources.as_ref().unwrap();
        assert_eq!(aur_sources.len(), 2);
        assert_eq!(aur_sources[0].name.as_deref(), Some("myapp"));
        assert_eq!(
            aur_sources[0].makedepends.as_ref().unwrap(),
            &["rust", "cargo"]
        );
        assert_eq!(aur_sources[1].name.as_deref(), Some("myapp-extra"));
    }

    #[test]
    fn test_aur_source_config_parsing() {
        use anodizer_core::config::Config;

        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur_source:
        name: myapp
        description: "My application"
        license: MIT
        makedepends:
          - rust
          - cargo
        depends:
          - openssl
        git_url: "ssh://aur@aur.archlinux.org/myapp.git"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let aur_src = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .aur_source
            .as_ref()
            .unwrap();
        assert_eq!(aur_src.name.as_deref(), Some("myapp"));
        assert_eq!(aur_src.makedepends.as_ref().unwrap(), &["rust", "cargo"]);
        assert_eq!(aur_src.depends.as_ref().unwrap(), &["openssl"]);
    }

    #[test]
    fn test_aur_source_amd64_variant_field_parses() {
        // amd64_variant lands on AurSourceConfig as a typed Amd64Variant enum
        // (PKGBUILD `prepare:` / `build:` / `package:` template surface uses
        // it as the `Amd64` var; AUR source pkgs don't filter binaries).
        use anodizer_core::config::{Amd64Variant, Config};

        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur_source:
        name: myapp
        description: "My application"
        license: MIT
        amd64_variant: v3
        git_url: "ssh://aur@aur.archlinux.org/myapp.git"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let aur_src = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .aur_source
            .as_ref()
            .unwrap();
        assert_eq!(aur_src.amd64_variant, Some(Amd64Variant::V3));
        assert_eq!(aur_src.amd64_variant.unwrap().as_str(), "v3");
    }

    #[test]
    fn test_aur_source_amd64_variant_typo_rejected() {
        // Typed enum constraint: anything outside v1/v2/v3/v4 must fail at
        // parse time so the bad value never silently lands in the PKGBUILD.
        use anodizer_core::config::Config;

        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur_source:
        name: myapp
        amd64_variant: v9000
"#;
        let result: Result<Config, serde_yaml_ng::Error> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "amd64_variant: v9000 must be rejected by the typed enum"
        );
    }

    /// Regression:
    /// `aur_sources[*].skip_upload: "{{ .IsSnapshot }}"` must
    /// template-expand before its bool/auto/empty interpretation. On
    /// a snapshot run the rendered value is `"true"` and the publish
    /// path must skip the entry without touching git.
    #[test]
    fn aur_sources_skip_upload_template_expands_to_true_on_snapshot() {
        use anodizer_core::config::{AurSourceConfig, Config, StringOrBool};
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.aur_sources = Some(vec![AurSourceConfig {
            // git_url intentionally unset — should_skip_publisher must
            // short-circuit before this becomes a problem.
            description: Some("a thing".to_string()),
            skip_upload: Some(StringOrBool::String("{{ .IsSnapshot }}".to_string())),
            ..Default::default()
        }]);

        let mut ctx = Context::new(
            config,
            ContextOptions {
                snapshot: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("IsSnapshot", "true");

        let log = StageLogger::new("publish", Verbosity::Normal);
        publish_top_level_aur_sources(&mut ctx, &log).expect(
            "skip_upload='{{ .IsSnapshot }}' on snapshot must skip the \
             entry without reaching the git push path (GR cba5b9f)",
        );
    }

    #[test]
    fn generate_source_srcinfo_omits_url_when_homepage_empty() {
        let meta = AurMeta {
            name: "myapp",
            version: "2.3.0",
            pkgrel: 2,
            description: "No homepage tool",
            homepage: "",
            license: &["Apache-2.0".to_string()],
            arches: &["x86_64".to_string(), "aarch64".to_string()],
        };
        let optdepends = vec!["bash-completion: shell completions".to_string()];
        let deps = AurDeps {
            depends: &[],
            makedepends: &[],
            optdepends: &optdepends,
            conflicts: &[],
            provides: &[],
        };
        let srcinfo = generate_source_srcinfo(&meta, &deps, "https://example.com/src-2.3.0.tar.gz");

        // empty homepage -> NO `url =` line.
        assert!(
            !srcinfo.contains("\turl ="),
            "url line must be omitted for empty homepage:\n{srcinfo}"
        );
        // optdepends rendered.
        assert!(srcinfo.contains("\toptdepends = bash-completion: shell completions"));
        // both fixed arches always present.
        assert!(srcinfo.contains("\tarch = x86_64"));
        assert!(srcinfo.contains("\tarch = aarch64"));
        assert!(srcinfo.contains("\tlicense = Apache-2.0"));
        assert!(srcinfo.contains("\tsource = https://example.com/src-2.3.0.tar.gz"));
        assert!(srcinfo.contains("\tsha256sums = SKIP"));
    }

    #[test]
    fn resolve_aur_source_package_name_strip_bin_honors_explicit_name() {
        use anodizer_core::config::AurSourceConfig;
        // Explicit name is taken verbatim, then -bin stripped when requested.
        let cfg = AurSourceConfig {
            name: Some("widget-bin".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_aur_source_package_name(&cfg, "ignored", true),
            "widget"
        );
        // strip disabled -> -bin retained.
        assert_eq!(
            resolve_aur_source_package_name(&cfg, "ignored", false),
            "widget-bin"
        );
    }

    // -----------------------------------------------------------------------
    // render_aur_source_inner — the skip-unaware render the live publish path
    // and the offline validator share. Pure (reads ctx, no git): covers source
    // URL derivation (GitURL owner extraction for both `://` and `git@host:`
    // remotes), the empty-owner warn, the `url_template` override + `Amd64`
    // scoping, and the dependency/field defaults landing in the rendered
    // PKGBUILD/.SRCINFO.
    // -----------------------------------------------------------------------

    use anodizer_core::config::{Config, CrateConfig, PublishConfig, StringOrBool};
    use anodizer_core::context::ContextOptions;
    use anodizer_core::log::Verbosity;

    fn quiet_log() -> StageLogger {
        StageLogger::new("publish", Verbosity::Quiet)
    }

    /// A bare context with the four template vars `render_aur_source_inner`
    /// reads (`Version`, `Tag`, `GitURL`, `ProjectName`). The default source
    /// URL is `https://github.com/<owner>/<project>/archive/refs/tags/<tag>.tar.gz`,
    /// so `owner` comes from `GitURL` and `project` from `ProjectName`.
    fn source_ctx(git_url: &str, project: &str, version: &str, tag: &str) -> Context {
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.template_vars_mut().set("Version", version);
        ctx.template_vars_mut().set("Tag", tag);
        ctx.template_vars_mut().set("GitURL", git_url);
        ctx.template_vars_mut().set("ProjectName", project);
        ctx
    }

    /// Default source URL: owner extracted from an `https://` GitURL
    /// (`split('/').nth(3)`), project from `ProjectName`, tag from `Tag`. The
    /// PKGBUILD `pkgver` carries the `Version` var with hyphens underscored.
    #[test]
    fn render_inner_default_url_from_https_giturl() {
        let ctx = source_ctx(
            "https://github.com/myorg/mytool.git",
            "mytool",
            "1.2.3-rc1",
            "v1.2.3-rc1",
        );
        let cfg = AurSourceConfig {
            description: Some("A source tool".to_string()),
            license: Some("MIT".to_string()),
            ..Default::default()
        };
        let render =
            render_aur_source_inner(&ctx, &cfg, "mytool", false, "aur_source", &quiet_log())
                .expect("render ok");
        assert_eq!(render.pkg_name, "mytool");
        // Default source URL points at the github archive tarball.
        assert!(
            render.rendered.pkgbuild.contains(
                "source=(\"https://github.com/myorg/mytool/archive/refs/tags/v1.2.3-rc1.tar.gz\")"
            ),
            "default source URL must derive owner from GitURL + project from ProjectName:\n{}",
            render.rendered.pkgbuild
        );
        // Version hyphen → underscore per AUR pkgver rules.
        assert!(
            render.rendered.pkgbuild.contains("pkgver='1.2.3_rc1'"),
            "{}",
            render.rendered.pkgbuild
        );
        // Default makedepends are rust + cargo.
        assert!(
            render
                .rendered
                .pkgbuild
                .contains("makedepends=('rust' 'cargo')"),
            "{}",
            render.rendered.pkgbuild
        );
        // conflicts/provides default to the bare package name.
        assert!(render.rendered.pkgbuild.contains("conflicts=('mytool')"));
        assert!(render.rendered.pkgbuild.contains("provides=('mytool')"));
    }

    /// Templated `description` / `homepage` (`{{ .Tag }}`) are
    /// template-rendered into the source PKGBUILD `pkgdesc=` / `url=` lines
    /// (and the .SRCINFO `pkgdesc =` / `url =` lines) — the literal delimiters
    /// must NOT leak. Regression for the raw-emit description/homepage bug.
    #[test]
    fn render_inner_description_and_homepage_templates_are_rendered() {
        let ctx = source_ctx(
            "https://github.com/myorg/mytool.git",
            "mytool",
            "1.2.3",
            "v1.2.3",
        );
        let cfg = AurSourceConfig {
            description: Some("mytool {{ .Tag }} source build".to_string()),
            homepage: Some("https://example.com/releases/{{ .Tag }}".to_string()),
            license: Some("MIT".to_string()),
            ..Default::default()
        };
        let render =
            render_aur_source_inner(&ctx, &cfg, "mytool", false, "aur_source", &quiet_log())
                .expect("render ok");
        assert!(
            render
                .rendered
                .pkgbuild
                .contains("pkgdesc=\"mytool v1.2.3 source build\""),
            "templated description must render into PKGBUILD pkgdesc=:\n{}",
            render.rendered.pkgbuild
        );
        assert!(
            render
                .rendered
                .pkgbuild
                .contains("url='https://example.com/releases/v1.2.3'"),
            "templated homepage must render into PKGBUILD url=:\n{}",
            render.rendered.pkgbuild
        );
        assert!(
            !render.rendered.pkgbuild.contains("{{"),
            "PKGBUILD must carry no unrendered `{{{{`:\n{}",
            render.rendered.pkgbuild
        );
        assert!(
            render
                .rendered
                .srcinfo
                .contains("pkgdesc = mytool v1.2.3 source build"),
            ".SRCINFO pkgdesc must carry the resolved value:\n{}",
            render.rendered.srcinfo
        );
        assert!(
            render
                .rendered
                .srcinfo
                .contains("url = https://example.com/releases/v1.2.3"),
            ".SRCINFO url must carry the resolved value:\n{}",
            render.rendered.srcinfo
        );
        assert!(
            !render.rendered.srcinfo.contains("{{"),
            ".SRCINFO must carry no unrendered `{{{{`:\n{}",
            render.rendered.srcinfo
        );
    }

    /// Default source URL owner extraction for an SCP-style `git@host:owner/repo`
    /// remote (the `contains(':')` branch, `split(':').nth(1).split('/').next()`).
    #[test]
    fn render_inner_default_url_from_scp_giturl() {
        let ctx = source_ctx(
            "git@github.com:acme/widget.git",
            "widget",
            "2.0.0",
            "v2.0.0",
        );
        let cfg = AurSourceConfig::default();
        let render =
            render_aur_source_inner(&ctx, &cfg, "widget", false, "aur_source", &quiet_log())
                .expect("render ok");
        assert!(
            render.rendered.pkgbuild.contains(
                "source=(\"https://github.com/acme/widget/archive/refs/tags/v2.0.0.tar.gz\")"
            ),
            "SCP-style GitURL owner must extract to 'acme':\n{}",
            render.rendered.pkgbuild
        );
    }

    /// An unparseable GitURL (no scheme, no `:`) yields an empty owner; the
    /// renderer warns and still produces a (malformed-owner) source URL rather
    /// than panicking.
    #[test]
    fn render_inner_empty_owner_warns_and_continues() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = source_ctx("not-a-url", "thing", "1.0.0", "v1.0.0");
        ctx.with_log_capture(capture.clone());
        let log = ctx.logger("publish");
        let cfg = AurSourceConfig::default();
        let render = render_aur_source_inner(&ctx, &cfg, "thing", false, "aur_source", &log)
            .expect("render ok despite unextractable owner");
        // Empty owner → URL has an empty owner segment.
        assert!(
            render
                .rendered
                .pkgbuild
                .contains("source=(\"https://github.com//thing/archive/refs/tags/v1.0.0.tar.gz\")"),
            "{}",
            render.rendered.pkgbuild
        );
        assert!(
            capture
                .warn_messages()
                .iter()
                .any(|m| m.contains("could not extract owner")),
            "an unextractable GitURL must warn the operator; got: {:?}",
            capture.warn_messages()
        );
    }

    /// `url_template` overrides the default github-archive URL and sees the
    /// `Amd64` micro-architecture var (default `v1`) plus the standard vars.
    #[test]
    fn render_inner_url_template_overrides_with_amd64_scope() {
        let ctx = source_ctx("https://github.com/o/p.git", "p", "3.1.0", "v3.1.0");
        let cfg = AurSourceConfig {
            url_template: Some(
                "https://dl.example/{{ .Version }}/{{ .Amd64 }}/src.tar.gz".to_string(),
            ),
            ..Default::default()
        };
        let render = render_aur_source_inner(&ctx, &cfg, "p", false, "aur_source", &quiet_log())
            .expect("render ok");
        assert!(
            render
                .rendered
                .pkgbuild
                .contains("source=(\"https://dl.example/3.1.0/v1/src.tar.gz\")"),
            "url_template must render with default Amd64=v1:\n{}",
            render.rendered.pkgbuild
        );
        // The scoped vars threaded out carry the same Amd64 the render saw.
        assert_eq!(
            render.scoped_vars.get("Amd64").map(|s| s.as_str()),
            Some("v1")
        );
    }

    /// A configured `amd64_variant` surfaces as the `Amd64` template var the
    /// `url_template` (and hook bodies) branch on.
    #[test]
    fn render_inner_amd64_variant_threads_into_template() {
        use anodizer_core::config::Amd64Variant;
        let ctx = source_ctx("https://github.com/o/p.git", "p", "1.0.0", "v1.0.0");
        let cfg = AurSourceConfig {
            amd64_variant: Some(Amd64Variant::V3),
            url_template: Some("https://dl/{{ .Amd64 }}.tar.gz".to_string()),
            ..Default::default()
        };
        let render = render_aur_source_inner(&ctx, &cfg, "p", false, "aur_source", &quiet_log())
            .expect("render ok");
        assert!(
            render
                .rendered
                .pkgbuild
                .contains("source=(\"https://dl/v3.tar.gz\")"),
            "{}",
            render.rendered.pkgbuild
        );
        assert_eq!(
            render.scoped_vars.get("Amd64").map(|s| s.as_str()),
            Some("v3")
        );
    }

    /// `install:` set → the render reports `<pkg>.install` and the PKGBUILD
    /// emits the `install=` line; unset → no install filename reference leaks
    /// into the body.
    #[test]
    fn render_inner_install_filename_tracks_config() {
        let ctx = source_ctx("https://github.com/o/p.git", "p", "1.0.0", "v1.0.0");
        let cfg = AurSourceConfig {
            install: Some("post_install() { :; }".to_string()),
            ..Default::default()
        };
        let render = render_aur_source_inner(&ctx, &cfg, "p", false, "aur_source", &quiet_log())
            .expect("render ok");
        assert_eq!(render.install_filename, "p.install");
        assert!(
            render.rendered.pkgbuild.contains("install=p.install"),
            "{}",
            render.rendered.pkgbuild
        );

        let cfg_none = AurSourceConfig::default();
        let render_none =
            render_aur_source_inner(&ctx, &cfg_none, "p", false, "aur_source", &quiet_log())
                .expect("render ok");
        assert!(
            !render_none.rendered.pkgbuild.contains("install="),
            "no install= line when install unset:\n{}",
            render_none.rendered.pkgbuild
        );
    }

    /// Top-level entries strip a trailing `-bin` from the default name; the
    /// `Version` default `0.0.0` applies when the var is absent.
    #[test]
    fn render_inner_strips_bin_and_defaults_version() {
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        // No Version var → defaults to 0.0.0; GitURL/ProjectName empty.
        ctx.template_vars_mut().set("Tag", "v9");
        let cfg = AurSourceConfig::default();
        let render =
            render_aur_source_inner(&ctx, &cfg, "foo-bin", true, "aur_sources[0]", &quiet_log())
                .expect("render ok");
        assert_eq!(
            render.pkg_name, "foo",
            "-bin must be stripped for top-level"
        );
        assert!(
            render.rendered.pkgbuild.contains("pkgver='0.0.0'"),
            "missing Version var must default to 0.0.0:\n{}",
            render.rendered.pkgbuild
        );
    }

    /// Workspace per-crate mode: an `aur_sources[]`/`aur_source` entry for crate
    /// `bravo` that omits description/homepage/license/maintainers must resolve
    /// each through `bravo`'s OWN `Cargo.toml` metadata — never crate `alfa`'s
    /// (no cross-crate leakage), never the crate name as description, never an
    /// empty url, and never a hardcoded `MIT` license. Mirrors the `-bin` AUR
    /// publisher's `meta_*_for(<crate>)` resolution.
    #[test]
    fn render_inner_per_crate_metadata_no_cross_crate_leakage() {
        use anodizer_core::config::MetadataConfig;

        let mut ctx = source_ctx("https://github.com/o/ws.git", "ws", "1.0.0", "v1.0.0");
        // Two crates' derived Cargo.toml metadata, as populate_derived_metadata
        // would key them. `bravo` carries a non-MIT license and a real homepage.
        ctx.config.derived_metadata.insert(
            "alfa".to_string(),
            MetadataConfig {
                description: Some("Alfa the first tool".to_string()),
                homepage: Some("https://alfa.example".to_string()),
                license: Some("Apache-2.0".to_string()),
                maintainers: Some(vec!["Alfa Author <alfa@example.com>".to_string()]),
                ..Default::default()
            },
        );
        ctx.config.derived_metadata.insert(
            "bravo".to_string(),
            MetadataConfig {
                description: Some("Bravo the second tool".to_string()),
                homepage: Some("https://bravo.example".to_string()),
                license: Some("GPL-3.0-or-later".to_string()),
                maintainers: Some(vec!["Bravo Author <bravo@example.com>".to_string()]),
                ..Default::default()
            },
        );

        // The `bravo` entry omits every metadata field — they must come from
        // bravo's own Cargo.toml, resolved by default_name = "bravo".
        let cfg = AurSourceConfig::default();
        let render =
            render_aur_source_inner(&ctx, &cfg, "bravo", false, "aur_source", &quiet_log())
                .expect("render ok");
        let pkgbuild = &render.rendered.pkgbuild;

        assert!(
            pkgbuild.contains("pkgdesc=\"Bravo the second tool\""),
            "description must be bravo's real Cargo.toml description, not the \
             crate name or alfa's:\n{}",
            pkgbuild
        );
        assert!(
            pkgbuild.contains("url='https://bravo.example'"),
            "homepage/url must be bravo's real Cargo.toml homepage, not empty or \
             alfa's:\n{}",
            pkgbuild
        );
        assert!(
            pkgbuild.contains("license=('GPL-3.0-or-later')"),
            "license must be bravo's real Cargo.toml license, not a hardcoded MIT \
             or alfa's:\n{}",
            pkgbuild
        );
        assert!(
            pkgbuild.contains("# Maintainer: Bravo Author <bravo@example.com>"),
            "maintainer must be bravo's real Cargo.toml author, not empty or \
             alfa's:\n{}",
            pkgbuild
        );

        // Prove no alfa leakage anywhere in the rendered artifact.
        assert!(
            !pkgbuild.contains("alfa")
                && !pkgbuild.contains("Alfa")
                && !pkgbuild.contains("Apache"),
            "crate alfa's metadata must not leak into bravo's PKGBUILD:\n{}",
            pkgbuild
        );
        assert!(
            !pkgbuild.contains("license=('MIT')"),
            "license must never fall back to a hardcoded MIT:\n{}",
            pkgbuild
        );

        // Explicit config still wins over the resolver: an entry that DOES set a
        // field uses the literal value, not the crate metadata.
        let cfg_explicit = AurSourceConfig {
            license: Some("BSD-3-Clause".to_string()),
            ..Default::default()
        };
        let render_explicit = render_aur_source_inner(
            &ctx,
            &cfg_explicit,
            "bravo",
            false,
            "aur_source",
            &quiet_log(),
        )
        .expect("render ok");
        assert!(
            render_explicit
                .rendered
                .pkgbuild
                .contains("license=('BSD-3-Clause')"),
            "explicit license must override the crate-metadata fallback:\n{}",
            render_explicit.rendered.pkgbuild
        );
    }

    // -----------------------------------------------------------------------
    // render_aur_source_pkgbuild_and_srcinfo_for_crate / render_top_level_aur_source
    // — the skip-aware entry points the offline validator drives. Pure (no git).
    // -----------------------------------------------------------------------

    fn crate_with_aur_source(name: &str, cfg: AurSourceConfig) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                aur_source: Some(cfg),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// No `aur_source` block on the crate → `Ok(None)` (the validator treats it
    /// as nothing to validate).
    #[test]
    fn render_per_crate_none_when_no_block() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig::default()),
            ..Default::default()
        }];
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut()
            .set("GitURL", "https://github.com/o/demo.git");
        ctx.template_vars_mut().set("ProjectName", "demo");
        let out = render_aur_source_pkgbuild_and_srcinfo_for_crate(&ctx, "demo", &quiet_log())
            .expect("render ok");
        assert!(out.is_none(), "no aur_source block → None");
    }

    /// A configured crate renders the PKGBUILD/.SRCINFO byte-content the live
    /// publish would push.
    #[test]
    fn render_per_crate_emits_pkgbuild() {
        let mut config = Config::default();
        config.crates = vec![crate_with_aur_source(
            "demo",
            AurSourceConfig {
                description: Some("demo tool".to_string()),
                ..Default::default()
            },
        )];
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut()
            .set("GitURL", "https://github.com/o/demo.git");
        ctx.template_vars_mut().set("ProjectName", "demo");
        let rendered = render_aur_source_pkgbuild_and_srcinfo_for_crate(&ctx, "demo", &quiet_log())
            .expect("render ok")
            .expect("not skipped");
        assert_eq!(rendered.package_name, "demo");
        assert!(
            rendered.pkgbuild.contains("pkgname='demo'"),
            "{}",
            rendered.pkgbuild
        );
        assert!(
            rendered.srcinfo.contains("pkgbase = demo"),
            "{}",
            rendered.srcinfo
        );
    }

    /// A truthy `skip` on the per-crate block short-circuits to `Ok(None)`.
    #[test]
    fn render_per_crate_skip_true_returns_none() {
        let mut config = Config::default();
        config.crates = vec![crate_with_aur_source(
            "demo",
            AurSourceConfig {
                skip: Some(StringOrBool::Bool(true)),
                ..Default::default()
            },
        )];
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut()
            .set("GitURL", "https://github.com/o/demo.git");
        ctx.template_vars_mut().set("ProjectName", "demo");
        let out = render_aur_source_pkgbuild_and_srcinfo_for_crate(&ctx, "demo", &quiet_log())
            .expect("render ok");
        assert!(out.is_none(), "skip=true → None");
    }

    /// Top-level `aur_sources` array: empty/unset → empty Vec; populated →
    /// one rendered artifact per non-skipped entry; a truthy `skip` drops the
    /// entry.
    #[test]
    fn render_top_level_handles_empty_populated_and_skip() {
        // Unset → empty Vec.
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.template_vars_mut()
            .set("GitURL", "https://github.com/o/p.git");
        ctx.template_vars_mut().set("ProjectName", "p");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("Version", "1.0.0");
        assert!(
            render_top_level_aur_source(&ctx, &quiet_log())
                .expect("render ok")
                .is_empty(),
            "unset aur_sources → empty Vec"
        );

        // Two entries, the second skipped → only the first renders.
        let mut config = Config::default();
        config.project_name = "p".to_string();
        config.aur_sources = Some(vec![
            AurSourceConfig {
                name: Some("first".to_string()),
                ..Default::default()
            },
            AurSourceConfig {
                name: Some("second".to_string()),
                skip: Some(StringOrBool::Bool(true)),
                ..Default::default()
            },
        ]);
        let mut ctx2 = Context::new(config, ContextOptions::default());
        ctx2.template_vars_mut()
            .set("GitURL", "https://github.com/o/p.git");
        ctx2.template_vars_mut().set("ProjectName", "p");
        ctx2.template_vars_mut().set("Tag", "v1.0.0");
        ctx2.template_vars_mut().set("Version", "1.0.0");
        let out = render_top_level_aur_source(&ctx2, &quiet_log()).expect("render ok");
        assert_eq!(out.len(), 1, "skipped entry must be dropped");
        assert_eq!(out[0].package_name, "first");
    }

    // -----------------------------------------------------------------------
    // Dry-run publish — exercises the `is_dry_run` early-exit in
    // `publish_aur_source_entry` without touching git.
    // -----------------------------------------------------------------------

    /// In dry-run, `publish_aur_source_entry` (via `publish_to_aur_source`)
    /// returns `Ok(false)` (nothing pushed) before any clone/write.
    #[test]
    fn publish_to_aur_source_dry_run_returns_false() {
        let mut config = Config::default();
        config.crates = vec![crate_with_aur_source(
            "demo",
            AurSourceConfig {
                description: Some("demo".to_string()),
                ..Default::default()
            },
        )];
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut()
            .set("GitURL", "https://github.com/o/demo.git");
        ctx.template_vars_mut().set("ProjectName", "demo");
        let pushed = publish_to_aur_source(&mut ctx, "demo", &quiet_log()).expect("dry-run ok");
        assert!(!pushed, "dry-run must not push");
    }

    /// `publish_to_aur_source` errors when the named crate carries no
    /// `aur_source` block at all (the `ok_or_else` on the missing config).
    #[test]
    fn publish_to_aur_source_missing_block_errors() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig::default()),
            ..Default::default()
        }];
        let mut ctx = Context::new(config, ContextOptions::default());
        let err = publish_to_aur_source(&mut ctx, "demo", &quiet_log())
            .expect_err("missing aur_source must error");
        assert!(
            format!("{err:#}").contains("no aur_source config"),
            "{err:#}"
        );
    }

    // -----------------------------------------------------------------------
    // Live git-over-ssh source publish — clone a local bare repo, write
    // PKGBUILD/.SRCINFO/.install, commit, push to `master`. `#[cfg(unix)]`-gated:
    // spawns git, sets commit-identity env, asserts pushed bytes on the bare
    // ref. Precedent: aur.rs `make_bare_aur_repo`.
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    fn ensure_git_identity() {
        use std::sync::OnceLock;
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            // SAFETY: runs once per process under OnceLock; constant values.
            unsafe {
                // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_AUTHOR_NAME", "Anodize Test");
                // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_AUTHOR_EMAIL", "test@anodize.local");
                // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_COMMITTER_NAME", "Anodize Test");
                // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_COMMITTER_EMAIL", "test@anodize.local");
                // env-ok: idempotent OnceLock set of constant git identity, never mutated after
                std::env::set_var("GIT_TERMINAL_PROMPT", "0");
            }
        });
    }

    #[cfg(unix)]
    fn git_ok(dir: &std::path::Path, args: &[&str]) {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.args(args).current_dir(dir);
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
    }

    #[cfg(unix)]
    fn git_stdout(dir: &std::path::Path, args: &[&str]) -> String {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.args(args).current_dir(dir);
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bare AUR repo seeded with one commit on `master`. Returns a usable
    /// local clone URL plus the holder tempdir.
    #[cfg(unix)]
    fn make_bare_aur_repo() -> (String, tempfile::TempDir) {
        ensure_git_identity();
        let bare = tempfile::tempdir().expect("bare tempdir");
        let seed = tempfile::tempdir().expect("seed tempdir");
        git_ok(bare.path(), &["init", "--bare", "-b", "master"]);
        git_ok(seed.path(), &["init", "-b", "master"]);
        git_ok(seed.path(), &["config", "user.email", "t@example.invalid"]);
        git_ok(seed.path(), &["config", "user.name", "T"]);
        git_ok(seed.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(seed.path().join("README"), "aur\n").unwrap();
        git_ok(seed.path(), &["add", "README"]);
        git_ok(seed.path(), &["commit", "-m", "seed"]);
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = std::process::Command::new("git");
                    cmd.args(["remote", "add", "origin"])
                        .arg(bare.path())
                        .current_dir(seed.path());
                    cmd
                },
                "git",
            )
            .status
            .success(),
            "git remote add failed"
        );
        git_ok(seed.path(), &["push", "-u", "origin", "master"]);
        (bare.path().to_string_lossy().into_owned(), bare)
    }

    /// Read a file as it landed on the bare repo's `master` ref.
    #[cfg(unix)]
    fn aur_show(bare: &std::path::Path, path: &str) -> String {
        git_stdout(bare, &["show", &format!("master:{path}")])
    }

    /// Build a per-crate source-publish context pointing the clone at a local
    /// bare repo, with the four template vars the render reads populated.
    #[cfg(unix)]
    fn live_source_ctx(bare_url: &str, cfg_mut: impl FnOnce(&mut AurSourceConfig)) -> Context {
        let mut cfg = AurSourceConfig {
            git_url: Some(bare_url.to_string()),
            description: Some("A source tool".to_string()),
            license: Some("MIT".to_string()),
            ..Default::default()
        };
        cfg_mut(&mut cfg);
        let mut config = Config::default();
        config.dist = std::env::temp_dir().join(format!(
            "anodize-aursrc-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        config.crates = vec![crate_with_aur_source("mytool", cfg)];
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.2.3");
        ctx.template_vars_mut().set("Tag", "v1.2.3");
        ctx.template_vars_mut()
            .set("GitURL", "https://github.com/myorg/mytool.git");
        ctx.template_vars_mut().set("ProjectName", "mytool");
        ctx
    }

    /// End-to-end per-crate source publish: clone, write, commit, push. Assert
    /// the pushed PKGBUILD pkgname + .SRCINFO pkgbase and the `true` outcome.
    #[cfg(unix)]
    #[test]
    fn publish_to_aur_source_pushes_to_master() {
        let (bare_url, bare) = make_bare_aur_repo();
        let mut ctx = live_source_ctx(&bare_url, |_| {});
        let pushed = publish_to_aur_source(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
        assert!(pushed, "a fresh source PKGBUILD must report a push");

        let pkgbuild = aur_show(std::path::Path::new(&bare_url), "PKGBUILD");
        assert!(pkgbuild.contains("pkgname='mytool'"), "{pkgbuild}");
        assert!(
            pkgbuild.contains(
                "source=(\"https://github.com/myorg/mytool/archive/refs/tags/v1.2.3.tar.gz\")"
            ),
            "{pkgbuild}"
        );
        let srcinfo = aur_show(std::path::Path::new(&bare_url), ".SRCINFO");
        assert!(srcinfo.contains("pkgbase = mytool"), "{srcinfo}");
        std::fs::remove_dir_all(&ctx.config.dist).ok();
        drop(bare);
    }

    /// A second publish against an unchanged repo reports `NoChanges` → `false`.
    #[cfg(unix)]
    #[test]
    fn publish_to_aur_source_second_run_no_changes_returns_false() {
        let (bare_url, bare) = make_bare_aur_repo();
        let mut ctx = live_source_ctx(&bare_url, |_| {});
        assert!(
            publish_to_aur_source(&mut ctx, "mytool", &quiet_log()).expect("first publish ok"),
            "first publish must push"
        );
        assert!(
            !publish_to_aur_source(&mut ctx, "mytool", &quiet_log()).expect("second publish ok"),
            "an unchanged repo must report no push"
        );
        std::fs::remove_dir_all(&ctx.config.dist).ok();
        drop(bare);
    }

    /// `install:` set → the `.install` file lands on `master` and the PKGBUILD
    /// references it. Also drives the `git_ssh_command` clone branch (a no-op
    /// `ssh` command; the local-path clone ignores `GIT_SSH_COMMAND`).
    #[cfg(unix)]
    #[test]
    fn publish_to_aur_source_writes_install_and_uses_ssh_branch() {
        let (bare_url, bare) = make_bare_aur_repo();
        let mut ctx = live_source_ctx(&bare_url, |c| {
            c.install = Some("post_install() { echo hi; }".to_string());
            // Non-empty git_ssh_command routes through `clone_repo_ssh`; for a
            // local-path clone git ignores GIT_SSH_COMMAND so the clone still
            // succeeds, exercising the SSH branch's config-write path.
            c.git_ssh_command = Some("ssh -o StrictHostKeyChecking=no".to_string());
        });
        assert!(publish_to_aur_source(&mut ctx, "mytool", &quiet_log()).expect("publish ok"));
        let pkgbuild = aur_show(std::path::Path::new(&bare_url), "PKGBUILD");
        assert!(pkgbuild.contains("install=mytool.install"), "{pkgbuild}");
        let install = aur_show(std::path::Path::new(&bare_url), "mytool.install");
        assert_eq!(install, "post_install() { echo hi; }");
        std::fs::remove_dir_all(&ctx.config.dist).ok();
        drop(bare);
    }

    /// `directory:` nests the committed files under a subdirectory rendered
    /// from the template (with the `Amd64` var scoped in).
    #[cfg(unix)]
    #[test]
    fn publish_to_aur_source_directory_nests_output() {
        let (bare_url, bare) = make_bare_aur_repo();
        let mut ctx = live_source_ctx(&bare_url, |c| {
            c.directory = Some("pkgs/{{ .Amd64 }}".to_string());
        });
        assert!(publish_to_aur_source(&mut ctx, "mytool", &quiet_log()).expect("publish ok"));
        // Amd64 defaults to v1, so the files land under pkgs/v1/.
        let pkgbuild = aur_show(std::path::Path::new(&bare_url), "pkgs/v1/PKGBUILD");
        assert!(pkgbuild.contains("pkgname='mytool'"), "{pkgbuild}");
        std::fs::remove_dir_all(&ctx.config.dist).ok();
        drop(bare);
    }

    /// Cloning a non-repo path fails; the error names the `aur_source` label.
    #[cfg(unix)]
    #[test]
    fn publish_to_aur_source_clone_failure_errors() {
        ensure_git_identity();
        let bogus = tempfile::tempdir().expect("bogus dir");
        let bogus_url = bogus.path().to_string_lossy().into_owned();
        let mut ctx = live_source_ctx(&bogus_url, |_| {});
        let err = publish_to_aur_source(&mut ctx, "mytool", &quiet_log())
            .expect_err("cloning a non-repo path must fail");
        assert!(
            format!("{err:#}").contains("aur_source"),
            "error must name the label: {err:#}"
        );
        std::fs::remove_dir_all(&ctx.config.dist).ok();
        drop(bogus);
    }

    /// Full `Publisher::run` over a per-crate source block pushes the package,
    /// records exactly one target carrying the pushed git_url + tag, and
    /// `rollback` warns (irreversible force-push) without error.
    #[cfg(unix)]
    #[test]
    fn aur_source_publisher_run_pushes_and_records_target() {
        use anodizer_core::Publisher;
        let (bare_url, bare) = make_bare_aur_repo();
        // Point project_root at a hermetic `v0.1.0`-tagged repo so the per-crate
        // scope resolves the crate's tag deterministically (its `tag_template`
        // is `v{{ .Version }}`), rather than depending on the process cwd's tags.
        let scope_repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = live_source_ctx(&bare_url, |_| {});
        ctx.options.project_root = Some(scope_repo.path().to_path_buf());
        ctx.options.selected_crates = vec!["mytool".to_string()];
        let p = AurSourcePublisher::new();

        let evidence = p.run(&mut ctx).expect("run ok");
        let targets = decode_aur_source_targets(&evidence.extra);
        assert_eq!(targets.len(), 1, "one push → one recorded target");
        assert_eq!(targets[0].package, "mytool");
        assert_eq!(targets[0].git_url, bare_url);
        assert_eq!(
            evidence.primary_ref.as_deref(),
            Some("https://aur.archlinux.org/packages/mytool"),
            "primary_ref must point at the AUR package page"
        );

        // The package landed on master.
        let pkgbuild = aur_show(std::path::Path::new(&bare_url), "PKGBUILD");
        assert!(pkgbuild.contains("pkgname='mytool'"), "{pkgbuild}");

        // Rollback is warn-only (force-push is irreversible); must not error.
        p.rollback(&mut ctx, &evidence).expect("rollback ok");
        std::fs::remove_dir_all(&ctx.config.dist).ok();
        drop(bare);
    }

    /// `Publisher::run` with a top-level `aur_sources` entry pushes it and
    /// records the `aur_sources[0]` target.
    #[cfg(unix)]
    #[test]
    fn aur_source_publisher_run_pushes_top_level_entry() {
        use anodizer_core::Publisher;
        let (bare_url, bare) = make_bare_aur_repo();
        let mut config = Config::default();
        config.project_name = "widget".to_string();
        config.dist = std::env::temp_dir().join(format!(
            "anodize-aursrc-top-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        config.aur_sources = Some(vec![AurSourceConfig {
            git_url: Some(bare_url.clone()),
            description: Some("widget tool".to_string()),
            ..Default::default()
        }]);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "2.0.0");
        ctx.template_vars_mut().set("Tag", "v2.0.0");
        ctx.template_vars_mut()
            .set("GitURL", "https://github.com/myorg/widget.git");
        ctx.template_vars_mut().set("ProjectName", "widget");
        let p = AurSourcePublisher::new();

        let evidence = p.run(&mut ctx).expect("run ok");
        let targets = decode_aur_source_targets(&evidence.extra);
        assert_eq!(targets.len(), 1, "one top-level entry → one target");
        assert_eq!(targets[0].target, "aur_sources[0]");
        assert_eq!(targets[0].package, "widget");

        let srcinfo = aur_show(std::path::Path::new(&bare_url), ".SRCINFO");
        assert!(srcinfo.contains("pkgbase = widget"), "{srcinfo}");
        std::fs::remove_dir_all(&ctx.config.dist).ok();
        drop(bare);
    }

    /// `aur_source.private_key` templates are rendered against the
    /// context env vars before the key bytes reach the SSH key file. A
    /// literal `{{ .Env.X }}` written to the file would fail `ssh` at
    /// parse time with "error in libcrypto". Mirrors the analogous
    /// `aur_clone_repo_renders_templated_private_key_before_write` test
    /// in `aur.rs`.
    #[cfg(unix)]
    #[test]
    fn aur_source_renders_templated_private_key_before_write() {
        let (bare_url, bare) = make_bare_aur_repo();
        let key_value =
            "-----BEGIN OPENSSH PRIVATE KEY-----\nZZZZ\n-----END OPENSSH PRIVATE KEY-----\n";

        // Build a context with the templated private_key and the env var
        // that the template references. `render_or_warn_with_vars` is the
        // same function `publish_to_aur_source` calls on `private_key`
        // before passing the rendered bytes to `clone_repo_ssh`.
        let mut ctx = live_source_ctx(&bare_url, |c| {
            c.private_key = Some("{{ .Env.AUR_SOURCE_TEST_KEY }}".to_string());
        });
        ctx.template_vars_mut()
            .set_env("AUR_SOURCE_TEST_KEY", key_value);

        // Render via the same path the production code takes.
        let log = quiet_log();
        let rendered = util::render_or_warn(
            &ctx,
            &log,
            "aur_source.private_key",
            "{{ .Env.AUR_SOURCE_TEST_KEY }}",
        )
        .expect("render must succeed when env var is set");
        assert_eq!(
            rendered, key_value,
            "rendered private_key must equal the env var value, not the literal template"
        );
        assert!(
            !rendered.contains("{{"),
            "the literal template must never appear in the rendered key"
        );

        // Also verify the full publish path: clone the bare repo with the
        // rendered key so the key file is actually written to disk. Since
        // the clone is local-path, `GIT_SSH_COMMAND` is ignored and the
        // clone succeeds regardless of key validity, letting us confirm
        // the render → write path end-to-end without a real SSH server.
        let parent = tempfile::tempdir().expect("parent");
        let dest = parent.path().join("clone");
        util::clone_repo_ssh(&bare_url, Some(&rendered), None, &dest, "aur_source", &log)
            .expect("clone with rendered key must succeed");
        let key_path = dest.join(".git").join("anodizer_ssh_key");
        let written = std::fs::read_to_string(&key_path).expect("persisted key must be written");
        assert_eq!(
            written.trim_end_matches('\n'),
            key_value.trim_end_matches('\n'),
            "key file must contain the rendered env var value, never the literal template"
        );
        assert!(
            !written.contains("{{"),
            "literal template must never reach the SSH key file"
        );

        std::fs::remove_dir_all(&ctx.config.dist).ok();
        drop(bare);
        drop(parent);
    }

    /// `Publisher::run` in dry-run records no targets (no push happened).
    #[test]
    fn aur_source_publisher_run_dry_run_records_no_targets() {
        use anodizer_core::Publisher;
        let mut config = Config::default();
        config.crates = vec![crate_with_aur_source(
            "demo",
            AurSourceConfig {
                git_url: Some("ssh://aur@aur.archlinux.org/demo.git".to_string()),
                description: Some("demo".to_string()),
                ..Default::default()
            },
        )];
        // Point project_root at a hermetic `v0.1.0`-tagged repo so the per-crate
        // scope resolves "demo"'s tag (`v{{ .Version }}`) deterministically
        // rather than from the process cwd's tags, which a checkout with no
        // fetched tags (CI) leaves empty — starving the resolution.
        let scope_repo = crate::testing::hermetic_tagged_repo();
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                project_root: Some(scope_repo.path().to_path_buf()),
                ..Default::default()
            },
        );
        ctx.options.selected_crates = vec!["demo".to_string()];
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut()
            .set("GitURL", "https://github.com/o/demo.git");
        ctx.template_vars_mut().set("ProjectName", "demo");
        let p = AurSourcePublisher::new();
        let evidence = p.run(&mut ctx).expect("dry-run run ok");
        let targets = decode_aur_source_targets(&evidence.extra);
        assert!(
            targets.is_empty(),
            "dry-run must not record force-push targets: {targets:?}"
        );
    }
}
