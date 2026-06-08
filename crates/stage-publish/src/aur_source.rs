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

    let description = cfg.description.as_deref().unwrap_or(default_name);
    let homepage = cfg.homepage.as_deref().unwrap_or("");
    let license = cfg.license.as_deref().unwrap_or("MIT");

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
                "{}: could not extract owner from GitURL; set url_template explicitly",
                label
            ));
        }
        format!("https://github.com/{owner}/{project}/archive/refs/tags/{tag}.tar.gz",)
    };

    let maintainers = cfg.maintainers.clone().unwrap_or_default();
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
        description,
        homepage,
        license,
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
        util::clone_repo_ssh(
            &git_url,
            cfg.private_key.as_deref(),
            cfg.git_ssh_command.as_deref(),
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
        let rendered_dir = util::render_or_warn_with_vars(&scoped_vars, log, label, dir);
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
    );
    let commit_opts = util::resolve_commit_opts(ctx, cfg.commit_author.as_ref());
    let outcome =
        util::commit_and_push_with_opts(repo_path, &["."], &commit_msg, None, label, &commit_opts)?;
    match outcome {
        util::CommitOutcome::Pushed => {
            log.status(&format!(
                "{}: package '{}' pushed to {}",
                label, pkg_name, git_url
            ));
        }
        util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "{}: nothing to push, package '{}' already up to date",
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
    license: &'a str,
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
    lines.push("\tarch = x86_64".to_string());
    lines.push("\tarch = aarch64".to_string());
    lines.push(format!("\tlicense = {}", license));
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
    lines.push("arch=('x86_64' 'aarch64')".to_string());
    if !homepage.is_empty() {
        lines.push(format!("url='{}'", homepage));
    }
    lines.push(format!("license=('{}')", license));

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
                "upstream-aur: force-push to '{}' at tag '{}' is irreversible \
                 without AUR maintainer coordination; verify state at \
                 https://aur.archlinux.org/packages/{} (git URL: {})",
                t.package, t.tag, t.package, t.git_url
            ));
        }
        log.status(&format!(
            "upstream-aur: {} force-push(es) recorded; irreversible",
            targets.len()
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
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
            license: "MIT",
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
            license: "MIT",
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
            license: "MIT",
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
            license: "MIT",
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
            license: "Apache-2.0",
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
}
