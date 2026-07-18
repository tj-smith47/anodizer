use anodizer_core::config::NixConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util;

use super::super::generate::{NixParams, generate_nix_expression};

use super::*;

/// Render and push the Nix derivation for `crate_name`.
///
/// Returns `Ok(true)` when an actual git push was made to the overlay
/// repo; `Ok(false)` when the publish was skipped (skip, skip_upload,
/// dry-run, or any future early-exit guard). The caller (Publisher::run)
/// uses the boolean to decide whether to record rollback evidence — see
/// `publish_to_homebrew` for the long-form rationale.
pub fn publish_to_nix(ctx: &mut Context, crate_name: &str, log: &StageLogger) -> Result<bool> {
    // Take owned copies of the per-crate config so the helpers below
    // are free to interleave their immutable reads with `&mut ctx`
    // template-render calls without violating the borrow checker.
    let (crate_cfg, nix_cfg) = {
        let (cc, publish) = crate::util::get_publish_config(ctx, crate_name, "nix")?;
        let nx = publish
            .nix
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("nix: no nix config for '{}'", crate_name))?
            .clone();
        (cc.clone(), nx)
    };
    let nix_cfg = &nix_cfg;
    let crate_cfg = &crate_cfg;

    if check_skip_guards(ctx, nix_cfg, crate_name, log)? {
        return Ok(false);
    }

    let RepoCoords {
        repo_owner,
        repo_name,
    } = resolve_repo_coords(ctx, nix_cfg, crate_name, log)?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would publish Nix expression for '{}' to {}/{}",
            crate_name, repo_owner, repo_name
        ));
        return Ok(false);
    }

    let version = ctx.version();

    // Single render source of truth: the same skip-unaware render the snapshot
    // validator runs. The skip / `if` / skip_upload gate above is evaluated
    // exactly once on this live path; `render_nix_derivation_inner` is itself
    // gate-free so it is never double-evaluated.
    let NixRender {
        name,
        expr: nix_expr,
        archives: _,
    } = render_nix_derivation_inner(ctx, crate_cfg, nix_cfg, crate_name, log)?;
    let name = name.as_str();
    util::guard_no_unrendered(ctx, log, "nix derivation", &nix_expr)?;

    let token = util::resolve_repo_token(ctx, nix_cfg.repository.as_ref(), Some("NIX_PKGS_TOKEN"));

    let tmp_dir = tempfile::tempdir().context("nix: create temp dir")?;
    let repo_path = tmp_dir.path();
    util::clone_repo(
        ctx,
        nix_cfg.repository.as_ref(),
        &repo_owner,
        &repo_name,
        token.as_deref(),
        repo_path,
        "nix",
        log,
    )?;

    let nix_path = nix_cfg
        .path
        .as_deref()
        .map(|p| p.to_string())
        .unwrap_or_else(|| format!("pkgs/{}/default.nix", name));
    let nix_file = repo_path.join(&nix_path);

    if let Some(parent) = nix_file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("nix: create dir {}", parent.display()))?;
    }

    std::fs::write(&nix_file, &nix_expr)
        .with_context(|| format!("nix: write {}", nix_file.display()))?;

    run_formatter(nix_cfg, &nix_file, log)?;

    log.status(&format!("wrote Nix expression {}", nix_file.display()));

    // (Re)generate the root `flake.nix`, merging this package into the
    // set recovered from any prior committed flake. Without a root flake
    // the overlay derivations are not flake-installable
    // (`nix profile install …#<name>` / `nix build .#<name>` /
    // `nix run …#<name>` have nothing to resolve). Merge-by-attr (rather
    // than re-globbing `pkgs/*`) keeps custom-`path` packages and prior
    // siblings intact across the per-crate re-clone loop. The attr is the
    // package name; the path is the derivation file actually written
    // (honoring `nix.path`).
    let flake_rel = super::super::flake::write_flake(repo_path, name, &nix_path)?;
    log.status(&format!(
        "wrote root flake {}",
        repo_path.join(flake_rel).display()
    ));

    finalize_publish(
        ctx,
        nix_cfg,
        repo_path,
        &[&nix_path, flake_rel],
        name,
        &version,
        &repo_owner,
        &repo_name,
        crate_name,
        log,
    )
}

/// Outcome of rendering a crate's Nix emission in-memory for snapshot
/// validation: the derivation `name`, the rendered expression, and the
/// `(nix_system, url, hash)` archive tuples the derivation maps. No repo
/// is cloned and no file is written — this is the validation-only twin of
/// [`publish_to_nix`]'s render path.
#[derive(Debug)]
pub(crate) struct NixRender {
    pub name: String,
    pub expr: String,
    pub archives: Vec<(String, String, String)>,
}

/// Render `crate_name`'s Nix derivation entirely in-memory so the snapshot
/// validator can assert it is well-formed and that every `packages.<system>`
/// maps a produced asset — WITHOUT mutating source, cloning the overlay
/// repo, or pushing. Mirrors the resolve/collect/generate path of
/// [`publish_to_nix`] up to (but not including) the clone.
///
/// Returns `Ok(None)` when the publisher would skip (skip / `if` falsy /
/// skip_upload), so the validator treats a skipped emission as nothing to
/// validate rather than a failure.
pub(crate) fn render_nix_for_validation(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<NixRender>> {
    let (crate_cfg, nix_cfg) = {
        let (cc, publish) = crate::util::get_publish_config(ctx, crate_name, "nix")?;
        let nx = publish
            .nix
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("nix: no nix config for '{}'", crate_name))?
            .clone();
        (cc.clone(), nx)
    };
    let nix_cfg = &nix_cfg;
    let crate_cfg = &crate_cfg;

    if check_skip_guards(ctx, nix_cfg, crate_name, log)? {
        return Ok(None);
    }

    let render = render_nix_derivation_inner(ctx, crate_cfg, nix_cfg, crate_name, log)?;
    Ok(Some(render))
}

/// `Ok(true)` when at least one release archive maps to a Nix system double
/// (`x86_64-linux` / `aarch64-darwin` / …) for `crate_name` — i.e. the
/// derivation's `src = fetchurl { … }` has at least one asset to point at.
/// `Ok(false)` when NO artifact maps to a Nix system (genuine absence): a
/// sharded / single-target snapshot that built no Nix-mappable archive, which
/// the snapshot validator treats as a skip rather than tripping the publisher's
/// "no Linux/Darwin archive artifacts" guard.
///
/// Distinguishes ABSENCE from ERROR by propagating the `Err`:
/// [`collect_platform_artifacts`] returns `Err` when a MATCHED artifact is
/// missing its `sha256` metadata — the bail fires upstream in
/// `util::artifacts::artifact_to_os_artifact`'s empty-sha256 guard (reached via
/// `find_all_platform_artifacts_with_variant`), the same metadata defect that
/// would otherwise embed an empty `sha256 = "";` the fixed-output derivation
/// cannot verify. That `Err` flows through here so the caller surfaces a
/// present-but-broken artifact rather than silently skipping it; only a clean
/// `Ok(empty)` (true absence) skips.
pub(crate) fn crate_has_nix_archive(
    ctx: &Context,
    nix_cfg: &NixConfig,
    crate_name: &str,
) -> Result<bool> {
    let all_artifacts = collect_platform_artifacts(ctx, crate_name, nix_cfg)?;
    Ok(all_artifacts
        .iter()
        .any(|a| nix_system_for_artifact(a).is_some()))
}

/// The skip-unaware render body shared by the live [`publish_to_nix`] path and
/// the snapshot validator: resolve metadata, collect platform artifacts, build
/// the `(nix_system, url, hash)` archive tuples, and render the `default.nix`
/// derivation expression. Carries NO skip / `if` / skip_upload gate — every
/// caller evaluates that gate exactly once before calling in, so it is never
/// double-evaluated.
pub(super) fn render_nix_derivation_inner(
    ctx: &Context,
    crate_cfg: &anodizer_core::config::CrateConfig,
    nix_cfg: &NixConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<NixRender> {
    let name_raw = nix_cfg.name.as_deref().unwrap_or(crate_name);
    let name = util::render_or_warn(ctx, log, "nix.name", name_raw)?;

    let version = ctx.version();
    let meta = resolve_nix_metadata(ctx, crate_cfg, nix_cfg, crate_name, log)?;

    let all_artifacts = collect_platform_artifacts(ctx, crate_name, nix_cfg)?;
    let archives = build_archive_tuples(&all_artifacts, nix_cfg, crate_name, &version, log)?;

    let needs_unzip = all_artifacts.iter().any(|a| a.url.ends_with(".zip"));
    let deps = nix_cfg.dependencies.as_deref().unwrap_or(&[]);
    let needs_make_wrapper = !deps.is_empty();
    let dep_args = unique_dep_args(deps);

    let install_lines = build_install_lines(nix_cfg, crate_cfg, &name, deps, needs_make_wrapper);
    let post_install_lines: Vec<String> = nix_cfg
        .post_install
        .as_ref()
        .map(|s| s.lines().map(|l| l.to_string()).collect())
        .unwrap_or_default();

    let (source_root, source_root_map) =
        resolve_source_roots(crate_cfg, &all_artifacts, &name, &version);

    let dynamically_linked = detect_dynamically_linked(ctx, crate_name)?;

    let expr = generate_nix_expression(&NixParams {
        name: &name,
        version: &version,
        description: meta.description.as_str(),
        long_description: meta.long_description.as_str(),
        homepage: meta.homepage.as_str(),
        changelog: meta.changelog.as_str(),
        license_expr: meta.license_expr.as_str(),
        maintainers: &meta.maintainers,
        main_program: meta.main_program.as_str(),
        archives: &archives,
        install_lines: &install_lines,
        post_install_lines: &post_install_lines,
        needs_unzip,
        needs_make_wrapper,
        dep_args: &dep_args,
        source_root: source_root.as_deref(),
        source_root_map: source_root_map.as_deref(),
        dynamically_linked,
    })?;

    Ok(NixRender {
        name,
        expr,
        archives,
    })
}

// ---------------------------------------------------------------------------
// Skip / repo / metadata helpers
// ---------------------------------------------------------------------------

/// Carrier for the two repo coordinates after template rendering.
#[derive(Debug)]
pub(super) struct RepoCoords {
    pub(super) repo_owner: String,
    pub(super) repo_name: String,
}

/// Bundle of rendered `meta.*` strings ready to feed into `NixParams`.
#[derive(Debug)]
pub(super) struct NixMetadata {
    pub(super) description: String,
    pub(super) long_description: String,
    pub(super) homepage: String,
    pub(super) changelog: String,
    /// Pre-rendered RHS of `meta.license` (after `license = `, no trailing
    /// `;`). Empty suppresses the attribute. Built via
    /// [`super::super::license::resolve_nix_license_meta`] so the only-valid-attr
    /// guard lives in one place.
    pub(super) license_expr: String,
    /// nixpkgs maintainer handles for `meta.maintainers` (rendered as a list;
    /// empty stays present-but-empty).
    pub(super) maintainers: Vec<String>,
    pub(super) main_program: String,
}

/// Returns `true` when any skip guard (config `skip`, falsy `if`, or
/// `skip_upload`) fires. Each guard emits its own operator-facing
/// `log.status` line before returning, so the caller needs only the
/// boolean — the specific reason is already in the log.
pub(super) fn check_skip_guards(
    ctx: &Context,
    nix_cfg: &NixConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<bool> {
    if let Some(d) = nix_cfg.skip.as_ref() {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("nix: render skip template for '{}'", crate_name))?;
        if off {
            log.status(&format!(
                "skipped nix config for '{}' — skip evaluates true",
                crate_name
            ));
            return Ok(true);
        }
    }
    let proceed = anodizer_core::config::evaluate_if_condition(
        nix_cfg.if_condition.as_deref(),
        &format!("nix publisher for crate '{}'", crate_name),
        |t| ctx.render_template(t),
    )?;
    if !proceed {
        log.status(&format!(
            "skipped nix for '{}' — `if` condition evaluated falsy",
            crate_name
        ));
        return Ok(true);
    }
    if util::should_skip_upload(
        nix_cfg.skip_upload.as_ref(),
        ctx,
        log,
        Some(&format!("nix for '{crate_name}'")),
    )? {
        return Ok(true);
    }
    Ok(false)
}

/// Resolves `(owner, name)` from the repository config and renders both
/// halves through the template engine.
pub(super) fn resolve_repo_coords(
    ctx: &Context,
    nix_cfg: &NixConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<RepoCoords> {
    let (repo_owner_raw, repo_name_raw) =
        crate::util::resolve_repo_owner_name(nix_cfg.repository.as_ref())
            .ok_or_else(|| anyhow::anyhow!("nix: no repository config for '{}'", crate_name))?;
    let repo_owner = util::render_or_warn(ctx, log, "nix.repository.owner", &repo_owner_raw)?;
    let repo_name = util::render_or_warn(ctx, log, "nix.repository.name", &repo_name_raw)?;
    Ok(RepoCoords {
        repo_owner,
        repo_name,
    })
}

/// Resolves `description`, `homepage`, `license`, and `main_program`
/// from the nix config with project-`metadata.*` fallback and template
/// rendering. Empty strings are valid sentinels that suppress the
/// corresponding `meta.<field>` attribute in the Tera template.
pub(super) fn resolve_nix_metadata(
    ctx: &Context,
    crate_cfg: &anodizer_core::config::CrateConfig,
    nix_cfg: &NixConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<NixMetadata> {
    let description_raw = nix_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description_for(crate_name))
        .unwrap_or("");
    let description = util::render_or_warn(ctx, log, "nix.description", description_raw)?;

    // `longDescription` is optional and has no Cargo.toml-derived source, so it
    // is emitted only when the user supplies `nix.long_description`.
    let long_description_raw = nix_cfg.long_description.as_deref().unwrap_or("");
    let long_description =
        util::render_or_warn(ctx, log, "nix.long_description", long_description_raw)?;

    let homepage_raw = nix_cfg
        .homepage
        .as_deref()
        .or_else(|| ctx.config.meta_homepage_for(crate_name))
        .unwrap_or("");
    let homepage = ctx
        .render_template(homepage_raw)
        .with_context(|| format!("nix: render homepage template for '{}'", crate_name))?;

    let changelog = resolve_nix_changelog(ctx, crate_cfg, nix_cfg, log)?;

    // The raw value can be a nix `lib.licenses` attribute (config-supplied,
    // a direct nix attr), a single SPDX id, OR a dual/compound SPDX expression
    // derived from `Cargo.toml` `[package].license`. `resolve_nix_license_meta`
    // maps a single known id to `lib.licenses.<attr>`, an `OR`/`AND` list of
    // known ids to `with lib.licenses; [ … ]`, and degrades any unknown id or
    // unparseable compound to the verbatim string form (always valid in
    // `meta`) rather than emit a bogus attr-path. Empty suppresses the field.
    let license_raw = nix_cfg
        .license
        .as_deref()
        .or_else(|| ctx.config.meta_license_for(crate_name))
        .unwrap_or("");
    let license_expr = render_license_expr(license_raw);

    // `maintainers` are nixpkgs handles (from `lib.maintainers`), not the
    // `Name <email>` author strings `meta_maintainers_for` derives from
    // Cargo.toml — so this reads ONLY the explicit `nix.maintainers` config.
    // Absent/empty still renders `maintainers = [ ];` (present-but-empty),
    // which clears the nixpkgs-review "maintainers absent" rejection. Each
    // handle is rendered verbatim into the derivation, so validate it is a
    // bare Nix identifier here — a bad handle would break the list syntax.
    let maintainers = nix_cfg.maintainers.clone().unwrap_or_default();
    for handle in &maintainers {
        super::super::generate::validate_maintainer_handle(handle)
            .with_context(|| format!("nix: invalid maintainer handle for '{}'", crate_name))?;
    }

    let main_program_raw = nix_cfg.main_program.as_deref().unwrap_or("");
    let main_program = ctx
        .render_template(main_program_raw)
        .with_context(|| format!("nix: render main_program template for '{}'", crate_name))?;

    Ok(NixMetadata {
        description,
        long_description,
        homepage,
        changelog,
        license_expr,
        maintainers,
        main_program,
    })
}

/// Render the `meta.license` right-hand side for a raw license value (an SPDX
/// id, an `OR`/`AND` expression, or a direct nix attr). Returns the empty
/// string for an empty input (which suppresses `meta.license`). See
/// [`super::super::license::resolve_nix_license_meta`] for the mapping/fallback rules.
pub(super) fn render_license_expr(license_raw: &str) -> String {
    use super::super::license::NixLicense;
    match super::super::license::resolve_nix_license_meta(license_raw) {
        None => String::new(),
        Some(NixLicense::Single(attr)) => format!("lib.licenses.{attr}"),
        Some(NixLicense::List(attrs)) => {
            format!("with lib.licenses; [ {} ]", attrs.join(" "))
        }
        Some(NixLicense::Str(s)) => {
            format!("\"{}\"", super::super::generate::nix_escape_string(&s))
        }
    }
}

/// Resolve `meta.changelog`. An explicit `nix.changelog` (templated) wins;
/// otherwise derive `<host>/<owner>/<repo>/releases/tag/<tag>` from the
/// crate's `release` repository and the release tag — the form ripgrep/fd use
/// in nixpkgs. Returns the empty string (suppressing the attribute) only when
/// neither an explicit value nor a derivable release repo is available.
pub(super) fn resolve_nix_changelog(
    ctx: &Context,
    crate_cfg: &anodizer_core::config::CrateConfig,
    nix_cfg: &NixConfig,
    log: &StageLogger,
) -> Result<String> {
    if let Some(raw) = nix_cfg.changelog.as_deref() {
        return util::render_or_warn(ctx, log, "nix.changelog", raw);
    }
    let Some((owner, repo, base)) = release_repo_coords(ctx, crate_cfg) else {
        return Ok(String::new());
    };
    // Prefer the resolved git tag (e.g. `v1.2.3` / `core-v0.3.2`); fall back to
    // `v<version>` when no tag var is set (snapshot/in-memory render paths).
    let tag = ctx
        .template_vars()
        .get("Tag")
        .cloned()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| {
            let v = ctx.version();
            if v.is_empty() {
                String::new()
            } else {
                format!("v{v}")
            }
        });
    if tag.is_empty() {
        return Ok(String::new());
    }
    Ok(format!("{base}/{owner}/{repo}/releases/tag/{tag}"))
}

/// Resolve the release repo `(owner, repo, host_base)` for `crate_cfg`,
/// rendering owner/name templates. Returns `None` when no GitHub/GitLab/Gitea
/// release repo is configured (no host to build a changelog URL from).
fn release_repo_coords(
    ctx: &Context,
    crate_cfg: &anodizer_core::config::CrateConfig,
) -> Option<(String, String, String)> {
    let release = crate_cfg.release.as_ref()?;
    let (repo_cfg, base) = if let Some(gh) = release.github.as_ref() {
        (gh, "https://github.com")
    } else if let Some(gl) = release.gitlab.as_ref() {
        (gl, "https://gitlab.com")
    } else {
        (release.gitea.as_ref()?, "https://gitea.com")
    };
    let owner = ctx.render_template(&repo_cfg.owner).ok()?;
    let repo = ctx.render_template(&repo_cfg.name).ok()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo, base.to_string()))
}
