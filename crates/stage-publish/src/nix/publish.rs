//! `publish_to_nix` orchestrator — resolves config, gathers artifacts,
//! generates the Nix expression, and pushes it to the configured repo.

use std::path::Path;

use anodizer_core::config::{NixConfig, NixDependency};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util::{self, OsArtifact};

use super::generate::{NixParams, SourceRootEntry, generate_nix_expression, nix_system};
use super::hashing::hex_sha256_to_nix_base32;
use anodizer_core::elf::is_dynamically_linked;

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
    let flake_rel = super::flake::write_flake(repo_path, name, &nix_path)?;
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
fn render_nix_derivation_inner(
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
struct RepoCoords {
    repo_owner: String,
    repo_name: String,
}

/// Bundle of rendered `meta.*` strings ready to feed into `NixParams`.
#[derive(Debug)]
struct NixMetadata {
    description: String,
    long_description: String,
    homepage: String,
    changelog: String,
    /// Pre-rendered RHS of `meta.license` (after `license = `, no trailing
    /// `;`). Empty suppresses the attribute. Built via
    /// [`super::license::resolve_nix_license_meta`] so the only-valid-attr
    /// guard lives in one place.
    license_expr: String,
    /// nixpkgs maintainer handles for `meta.maintainers` (rendered as a list;
    /// empty stays present-but-empty).
    maintainers: Vec<String>,
    main_program: String,
}

/// Returns `true` when any skip guard (config `skip`, falsy `if`, or
/// `skip_upload`) fires. Each guard emits its own operator-facing
/// `log.status` line before returning, so the caller needs only the
/// boolean — the specific reason is already in the log.
fn check_skip_guards(
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
fn resolve_repo_coords(
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
fn resolve_nix_metadata(
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
        super::generate::validate_maintainer_handle(handle)
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
/// [`super::license::resolve_nix_license_meta`] for the mapping/fallback rules.
fn render_license_expr(license_raw: &str) -> String {
    use super::license::NixLicense;
    match super::license::resolve_nix_license_meta(license_raw) {
        None => String::new(),
        Some(NixLicense::Single(attr)) => format!("lib.licenses.{attr}"),
        Some(NixLicense::List(attrs)) => {
            format!("with lib.licenses; [ {} ]", attrs.join(" "))
        }
        Some(NixLicense::Str(s)) => format!("\"{}\"", super::generate::nix_escape_string(&s)),
    }
}

/// Resolve `meta.changelog`. An explicit `nix.changelog` (templated) wins;
/// otherwise derive `<host>/<owner>/<repo>/releases/tag/<tag>` from the
/// crate's `release` repository and the release tag — the form ripgrep/fd use
/// in nixpkgs. Returns the empty string (suppressing the attribute) only when
/// neither an explicit value nor a derivable release repo is available.
fn resolve_nix_changelog(
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

// ---------------------------------------------------------------------------
// Artifact + archive helpers
// ---------------------------------------------------------------------------

/// The nix system for a platform artifact, or `None` when it is not
/// nix-installable.
///
/// Wraps [`nix_system`] with the genuine-macOS check the raw `(os, arch)`
/// mapping cannot make on its own: `map_target` classifies every `*-apple-*`
/// triple as `os = "darwin"`, folding `aarch64-apple-ios` / `-tvos` /
/// `-watchos` in with real macOS. A nix darwin package built from a watchOS
/// archive is a failure-hiding emission — a `nix build` on `aarch64-darwin`
/// would fetch a binary that cannot run there. So a `darwin`-classified
/// artifact is nix-eligible only when its triple is genuine macOS
/// ([`is_macos`]); this mirrors homebrew's `is_macos || is_linux` artifact
/// filter. Linux is already precise (`map_target` never mislabels a non-Linux
/// triple `linux`), so it passes through untouched.
///
/// [`is_macos`]: anodizer_core::target::is_macos
fn nix_system_for_artifact(a: &OsArtifact) -> Option<String> {
    if a.os == "darwin" && !anodizer_core::target::is_macos(&a.target) {
        return None;
    }
    nix_system(&a.os, &a.arch)
}

/// Gathers all Linux/Darwin platform artifacts for the crate, applying
/// the configured ID filter and `amd64_variant` (defaulting to `v1`).
fn collect_platform_artifacts(
    ctx: &Context,
    crate_name: &str,
    nix_cfg: &NixConfig,
) -> anyhow::Result<Vec<OsArtifact>> {
    let ids_filter = nix_cfg.ids.as_deref();
    let amd64_variant = nix_cfg.amd64_variant.map_or("v1", |v| v.as_str());
    util::find_all_platform_artifacts_with_variant(
        ctx,
        crate_name,
        ids_filter,
        Some(amd64_variant),
        None,
    )
}

/// Builds the `(nix_system, download_url, base32_hash)` triples that
/// feed into the Tera template. Bails out before emitting an
/// unverifiable derivation if any nix-system artifact is missing its
/// `sha256` metadata. Warns and falls back to raw hex if the base32
/// conversion errors.
fn build_archive_tuples(
    all_artifacts: &[OsArtifact],
    nix_cfg: &NixConfig,
    crate_name: &str,
    version: &str,
    log: &StageLogger,
) -> Result<Vec<(String, String, String)>> {
    if let Some(empty) = all_artifacts
        .iter()
        .find(|a| nix_system_for_artifact(a).is_some() && a.sha256.is_empty())
    {
        anyhow::bail!(
            "nix: artifact for crate '{}' at url '{}' (os={}, arch={}) is \
             missing required sha256 metadata. The generated Nix derivation \
             would embed an empty `sha256 = \"\";`, which `nix-build` rejects \
             (the fetchurl fixed-output derivation cannot verify the source). \
             Check dist/artifacts.json for the archive entry's metadata.sha256 \
             and re-run `task release` from a clean dist/ if the field is \
             absent or empty.",
            crate_name,
            empty.url,
            empty.os,
            empty.arch,
        );
    }

    let url_template = nix_cfg.url_template.as_deref();
    // Multiple artifacts can map to one nix system (e.g. an Archive and an
    // UploadableBinary for the same target, or several archive formats). The
    // derivation's `urlMap`/`shaMap`/`src` and `meta.platforms` must each carry
    // exactly one entry per system, so dedup by nix system here at the source.
    // First occurrence wins (deterministic), matching the artifact ordering
    // (`Archive` kind precedes `UploadableBinary`); without this the BTreeMap
    // downstream collapsed urlMap last-writer-wins while `meta.platforms`
    // triplicated, an inconsistency that also broke output reproducibility.
    let mut seen_systems = std::collections::HashSet::new();
    let archives: Vec<(String, String, String)> = all_artifacts
        .iter()
        .filter_map(|a| {
            let system = nix_system_for_artifact(a)?;
            if !seen_systems.insert(system.clone()) {
                return None;
            }
            let download_url = if let Some(tmpl) = url_template {
                util::render_url_template(tmpl, crate_name, version, &a.arch, &a.os)
            } else {
                a.url.clone()
            };
            let nix_hash = match hex_sha256_to_nix_base32(&a.sha256) {
                Ok(h) => h,
                Err(e) => {
                    log.warn(&format!(
                        "failed to convert SHA256 to nix base32 for {}: {}; using raw hex",
                        a.url, e
                    ));
                    a.sha256.clone()
                }
            };
            Some((system, download_url, nix_hash))
        })
        .collect();

    if archives.is_empty() {
        anyhow::bail!(
            "nix: no Linux/Darwin archive artifacts found for '{}'",
            crate_name
        );
    }
    Ok(archives)
}

/// De-duplicates the dependency attribute names while preserving the
/// declaration order — these become the derivation function arguments.
fn unique_dep_args(deps: &[NixDependency]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    deps.iter()
        .filter(|d| seen.insert(d.name.clone()))
        .map(|d| d.name.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Install + sourceRoot + dyn-link detection
// ---------------------------------------------------------------------------

/// Builds the lines that compose the Nix `installPhase`. Falls back to
/// the auto-generated `mkdir -p $out/bin; cp …` block when no custom
/// `install` script is configured. Appends `wrapProgram` invocations
/// for OS-filtered dependencies when `makeWrapper` is needed.
fn build_install_lines(
    nix_cfg: &NixConfig,
    crate_cfg: &anodizer_core::config::CrateConfig,
    name: &str,
    deps: &[NixDependency],
    needs_make_wrapper: bool,
) -> Vec<String> {
    if let Some(ref custom_install) = nix_cfg.install {
        let mut lines: Vec<String> = custom_install.lines().map(|l| l.to_string()).collect();
        if let Some(ref extra) = nix_cfg.extra_install {
            lines.extend(extra.lines().map(|l| l.to_string()));
        }
        return lines;
    }

    let mut lines = vec!["mkdir -p $out/bin".to_string()];
    let bin_names = collect_binary_names(crate_cfg, name);
    for bin in &bin_names {
        lines.push(format!("cp -vr ./{bin} $out/bin/{bin}"));
        lines.push(format!("chmod +x $out/bin/{bin}"));
    }
    // Install shell completions / man pages the archive bundles. The archive
    // stage lays completions under `completions/` and man pages under
    // `man/man1/` (the `*Config::DEFAULT_DST` dirs) inside every archive, so
    // when the crate configures either block, the unpacked sourceRoot carries
    // those files and `installShellCompletion` / `installManPage` route them
    // into the derivation's `$out` rather than dropping them. Gated on the
    // archive config actually requesting them — mirrors how ripgrep/fd install
    // their completions/man in nixpkgs.
    lines.extend(build_completion_install_lines(crate_cfg, &bin_names));
    lines.extend(build_manpage_install_lines(crate_cfg));
    if let Some(ref extra) = nix_cfg.extra_install {
        lines.extend(extra.lines().map(|l| l.to_string()));
    }
    if needs_make_wrapper && let Some(wrap_line) = build_wrap_program_line(deps, name) {
        lines.push(wrap_line);
    }
    lines
}

/// Build `installShellCompletion` lines for any archive entry that bundles
/// completions. The archive stage writes one file per shell into the entry's
/// completions dir (default `completions/`) named per clap convention
/// (`<bin>` / `_<bin>` / `<bin>.fish`), so `installShellCompletion
/// --cmd <bin> --bash … --zsh … --fish …` picks each up by its on-disk name.
/// Only bash/zsh/fish have an `installShellFiles` flag, so other shells the
/// user generated are left in the archive (still distributed). Returns an
/// empty vec when no archive entry configures completions.
fn build_completion_install_lines(
    crate_cfg: &anodizer_core::config::CrateConfig,
    bin_names: &[String],
) -> Vec<String> {
    use anodizer_core::config::{ArchivesConfig, completion_filename};
    let ArchivesConfig::Configs(cfgs) = &crate_cfg.archives else {
        return Vec::new();
    };
    let primary_bin = bin_names.first().map(String::as_str).unwrap_or("");
    let mut lines = Vec::new();
    for cfg in cfgs {
        let Some(comp) = cfg.completions.as_ref() else {
            continue;
        };
        if matches!(comp.mode(), anodizer_core::config::GenMode::None) {
            continue;
        }
        let dst = comp.resolved_dst();
        let dir = dst.strip_suffix('/').unwrap_or(dst);
        // Only the three shells `installShellCompletion` natively flags.
        let mut flags = String::new();
        for (shell, flag) in [("bash", "--bash"), ("zsh", "--zsh"), ("fish", "--fish")] {
            if comp
                .resolved_shells()
                .iter()
                .any(|s| s.eq_ignore_ascii_case(shell))
            {
                let file = completion_filename(primary_bin, shell);
                flags.push_str(&format!(" {flag} {dir}/{file}"));
            }
        }
        if !flags.is_empty() {
            lines.push(format!("installShellCompletion --cmd {primary_bin}{flags}"));
        }
    }
    lines
}

/// Build `installManPage` lines for any archive entry that bundles man pages.
/// The archive stage writes man files into the entry's manpages dir (default
/// `man/man1/`), so a glob over that dir installs whatever the archive ships.
/// Returns an empty vec when no archive entry configures man pages.
fn build_manpage_install_lines(crate_cfg: &anodizer_core::config::CrateConfig) -> Vec<String> {
    use anodizer_core::config::{ArchivesConfig, GenMode};
    let ArchivesConfig::Configs(cfgs) = &crate_cfg.archives else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for cfg in cfgs {
        let Some(man) = cfg.manpages.as_ref() else {
            continue;
        };
        if matches!(man.mode(), GenMode::None) {
            continue;
        }
        let dst = man.resolved_dst();
        let dir = dst.strip_suffix('/').unwrap_or(dst);
        if seen.insert(dir.to_string()) {
            lines.push(format!("installManPage {dir}/*"));
        }
    }
    lines
}

/// Pulls binary names from each configured build, de-duplicated in
/// declaration order. Falls back to the derivation name when no builds
/// are configured.
fn collect_binary_names(crate_cfg: &anodizer_core::config::CrateConfig, name: &str) -> Vec<String> {
    let mut names: Vec<String> = crate_cfg
        .builds
        .as_ref()
        .map(|builds| {
            builds
                .iter()
                .filter_map(|b| b.binary.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    names.retain(|n| seen.insert(n.clone()));
    if names.is_empty() {
        names.push(name.to_string());
    }
    names
}

/// Builds the single `wrapProgram … --prefix PATH : ${lib.makeBinPath …}`
/// line that splices dependencies into the wrapped binary's PATH.
/// Partitions deps into darwin-only, linux-only, and all-OS buckets so
/// the generated expression uses `lib.optionals std…isDarwin` /
/// `…isLinux` guards. Returns `None` when no deps survive the partition.
fn build_wrap_program_line(deps: &[NixDependency], name: &str) -> Option<String> {
    let all_os_deps: Vec<&str> = deps
        .iter()
        .filter(|d| d.os.is_none())
        .map(|d| d.name.as_str())
        .collect();
    let darwin_deps: Vec<&str> = deps
        .iter()
        .filter(|d| d.os.as_deref() == Some("darwin"))
        .map(|d| d.name.as_str())
        .collect();
    let linux_deps: Vec<&str> = deps
        .iter()
        .filter(|d| d.os.as_deref() == Some("linux"))
        .map(|d| d.name.as_str())
        .collect();

    let mut list_parts: Vec<String> = Vec::new();
    if !darwin_deps.is_empty() {
        let items = darwin_deps.join(" ");
        list_parts.push(format!("lib.optionals stdenvNoCC.isDarwin [ {items} ]"));
    }
    if !linux_deps.is_empty() {
        let items = linux_deps.join(" ");
        list_parts.push(format!("lib.optionals stdenvNoCC.isLinux [ {items} ]"));
    }
    if !all_os_deps.is_empty() {
        let items = all_os_deps.join(" ");
        list_parts.push(format!("[ {items} ]"));
    }

    if list_parts.is_empty() {
        return None;
    }
    let joined = list_parts.join(" ++\n      ");
    Some(format!(
        "wrapProgram $out/bin/{name} --prefix PATH : ${{lib.makeBinPath (\n      {joined}\n    )}}"
    ))
}

/// Resolves the derivation's `sourceRoot` from each archive config's
/// `wrap_in_directory`. Returns a single `Some(root)` when every Nix
/// system maps to the same value, otherwise yields a per-system
/// `SourceRootEntry` list sorted by system identifier.
fn resolve_source_roots(
    crate_cfg: &anodizer_core::config::CrateConfig,
    all_artifacts: &[OsArtifact],
    name: &str,
    version: &str,
) -> (Option<String>, Option<Vec<SourceRootEntry>>) {
    let default_stem = format!("{}-{}", name, version);
    let archive_cfgs = match &crate_cfg.archives {
        anodizer_core::config::ArchivesConfig::Configs(cfgs) => cfgs.clone(),
        anodizer_core::config::ArchivesConfig::Disabled => vec![],
    };

    let mut per_system: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for art in all_artifacts {
        if let Some(system) = nix_system_for_artifact(art) {
            let wrap_dir = archive_cfgs
                .iter()
                .find(|cfg| match (&art.id, &cfg.id) {
                    (Some(aid), Some(cid)) => aid == cid,
                    (_, None) if archive_cfgs.len() == 1 => true,
                    _ => false,
                })
                .or_else(|| archive_cfgs.first())
                .and_then(|cfg| {
                    cfg.wrap_in_directory
                        .as_ref()
                        .and_then(|w| w.directory_name(&default_stem))
                })
                .unwrap_or_else(|| ".".to_string());
            per_system.insert(system, wrap_dir);
        }
    }

    let unique_roots: std::collections::HashSet<&str> =
        per_system.values().map(|s| s.as_str()).collect();

    if unique_roots.len() <= 1 {
        let single = per_system
            .values()
            .next()
            .cloned()
            .unwrap_or_else(|| ".".to_string());
        (Some(single), None)
    } else {
        let mut entries: Vec<SourceRootEntry> = per_system
            .into_iter()
            .map(|(system, root)| SourceRootEntry { system, root })
            .collect();
        entries.sort_by(|a, b| a.system.cmp(&b.system));
        (None, Some(entries))
    }
}

/// Returns `true` if any binary artifact for the crate is dynamically
/// linked. Prefers the build-stage metadata flag `DynamicallyLinked` to
/// avoid redundant disk I/O; falls back to direct ELF inspection for
/// artifacts that lack the marker.
fn detect_dynamically_linked(ctx: &Context, crate_name: &str) -> anyhow::Result<bool> {
    let binary_artifacts = ctx
        .artifacts
        .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Binary, crate_name);
    for a in &binary_artifacts {
        if let Some(v) = a.metadata.get("DynamicallyLinked") {
            if v == "true" {
                return Ok(true);
            }
            continue;
        }
        // A registered binary we cannot inspect must fail the nix publish, not
        // silently drop autoPatchelfHook and ship a broken derivation.
        if is_dynamically_linked(&a.path)
            .with_context(|| format!("inspecting {} for ELF dynamic linking", a.path.display()))?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Formatter + commit/push helpers
// ---------------------------------------------------------------------------

/// Runs the configured `alejandra` / `nixfmt` formatter against the
/// generated derivation. Formatting is opt-in (no `formatter` set is a
/// no-op, matching GoReleaser), but once a formatter IS configured it is
/// MANDATORY: a missing binary, a non-zero exit, or an unrecognized name
/// each `bail!`s so the unformatted derivation is never committed/pushed
/// to the external nix repo.
///
/// This is INTENTIONALLY stricter than GoReleaser, whose `nix.go::format`
/// only warns on failure — the "no unformatted push" requirement justifies
/// the divergence; the opt-in gating (format only when a formatter is set)
/// still matches GR.
fn run_formatter(nix_cfg: &NixConfig, nix_file: &Path, log: &StageLogger) -> Result<()> {
    let Some(ref formatter) = nix_cfg.formatter else {
        return Ok(());
    };
    match formatter.as_str() {
        "alejandra" | "nixfmt" => {}
        _ => {
            anyhow::bail!(
                "nix: unknown formatter '{}' (expected alejandra or nixfmt)",
                formatter
            );
        }
    }

    // Detect-and-fail-loud (no runtime auto-install) — consistent with
    // cosign/syft being required-present. The CI base image
    // (anodizer-action `install:`) provisions the formatter. A genuine probe
    // error (e.g. permission denied) surfaces as itself rather than the
    // misleading "not found on PATH" remedy.
    match anodizer_core::tool_detect::runs(formatter) {
        anodizer_core::tool_detect::ToolProbe::Available => {}
        anodizer_core::tool_detect::ToolProbe::Unavailable => {
            anyhow::bail!(
                "nix: formatter '{formatter}' not found on PATH — install it \
                 (anodizer-action install: list / CI base image) so the generated \
                 derivation is formatted before push"
            );
        }
        anodizer_core::tool_detect::ToolProbe::ProbeFailed(e) => {
            anyhow::bail!("nix: could not probe formatter '{formatter}' availability ({e})");
        }
    }

    let nix_file_str = nix_file.to_string_lossy();
    let output = std::process::Command::new(formatter)
        .arg(&*nix_file_str)
        .output()
        .with_context(|| format!("nix: spawn formatter '{formatter}'"))?;
    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "nix: {formatter} formatting failed for {} (exit {code}); \
             refusing to push an unformatted derivation\n{stderr}{stdout}",
            nix_file.display()
        );
    }
    log.status(&format!("formatted nix derivation with {formatter}"));
    Ok(())
}

/// Renders the commit message, commits + pushes the nix expression,
/// then optionally opens a PR. Returns `true` when an actual push
/// reached the remote (matches `publish_to_nix`'s rollback contract).
#[allow(clippy::too_many_arguments)]
fn finalize_publish(
    ctx: &mut Context,
    nix_cfg: &NixConfig,
    repo_path: &Path,
    files: &[&str],
    name: &str,
    version: &str,
    repo_owner: &str,
    repo_name: &str,
    crate_name: &str,
    log: &StageLogger,
) -> Result<bool> {
    let previous_tag = ctx
        .template_vars()
        .get("PreviousTag")
        .cloned()
        .unwrap_or_default();
    let commit_msg = crate::homebrew::render_commit_msg_with_prev(
        nix_cfg.commit_msg_template.as_deref(),
        name,
        version,
        &previous_tag,
        "nix",
        log,
        ctx.render_is_strict(),
    )?;
    let commit_opts = util::resolve_commit_opts(ctx, nix_cfg.commit_author.as_ref(), log)?;
    let branch = util::resolve_branch_or_versioned(ctx, nix_cfg.repository.as_ref(), name, version);
    let outcome = util::commit_and_push_with_opts(
        repo_path,
        files,
        &commit_msg,
        branch.as_deref(),
        "nix",
        &commit_opts,
        log,
    )?;

    // Clone the repository config so `maybe_submit_pr` no longer
    // borrows from `ctx.config` (via `nix_cfg`). NLL then drops the
    // immutable borrow, making the subsequent `&mut ctx` call legal.
    let repo_for_pr = nix_cfg.repository.clone();
    let pr_branch = branch.as_deref().unwrap_or("main").to_string();
    let pr_outcome = util::maybe_submit_pr(
        repo_path,
        repo_for_pr.as_ref(),
        &util::PrOrigin {
            repo_owner,
            repo_name,
            branch_name: &pr_branch,
            // Nix publishes commit directly to the expression repo
            // branch; the optional PR is informational. The
            // winget/krew/cask `update_existing_pr:` flag has no
            // analogue on `NixConfig` because there's no real
            // "blocked queue" to recover from here.
            update_existing_pr: false,
        },
        &format!("Update {} to {}", name, version),
        &format!(
            "## Package\n- **Name**: {}\n- **Version**: {}\n\n{}",
            name,
            version,
            crate::util::SUBMITTED_BY_FOOTER
        ),
        "nix",
        log,
        &|s| ctx.render_template(s).unwrap_or_else(|_| s.to_string()),
    );

    match outcome {
        util::CommitOutcome::Pushed => {
            log.status(&format!(
                "Nix expression pushed to {}/{} for '{}'",
                repo_owner, repo_name, crate_name
            ));
        }
        util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "nothing to push, nix expression for '{}' already up to date",
                crate_name
            ));
        }
    }

    if let Some(pr_outcome) = pr_outcome {
        ctx.record_publisher_outcome(pr_outcome);
    }

    Ok(outcome.is_pushed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{
        ArchiveConfig, ArchivesConfig, BuildConfig, CrateConfig, NixConfig, NixDependency,
        WrapInDirectory,
    };
    use anodizer_core::log::{StageLogger, Verbosity};

    fn quiet_log() -> StageLogger {
        StageLogger::new("publish", Verbosity::Quiet)
    }

    #[test]
    fn commit_outcome_is_pushed() {
        assert!(util::CommitOutcome::Pushed.is_pushed());
        assert!(!util::CommitOutcome::NoChanges.is_pushed());
    }

    // -----------------------------------------------------------------
    // unique_dep_args — declaration order preserved, dupes collapsed.
    // -----------------------------------------------------------------

    #[test]
    fn unique_dep_args_empty_returns_empty() {
        assert!(unique_dep_args(&[]).is_empty());
    }

    #[test]
    fn unique_dep_args_dedupes_preserving_first_occurrence_order() {
        let deps = vec![
            NixDependency {
                name: "openssl".to_string(),
                os: Some("linux".to_string()),
            },
            NixDependency {
                name: "openssl".to_string(),
                os: Some("darwin".to_string()),
            },
            NixDependency {
                name: "git".to_string(),
                os: None,
            },
            NixDependency {
                name: "openssl".to_string(),
                os: None,
            },
        ];
        assert_eq!(
            unique_dep_args(&deps),
            vec!["openssl".to_string(), "git".to_string()]
        );
    }

    // -----------------------------------------------------------------
    // collect_binary_names — pulled from builds, falls back to name.
    // -----------------------------------------------------------------

    #[test]
    fn collect_binary_names_falls_back_to_derivation_name_when_no_builds() {
        let cc = CrateConfig {
            builds: None,
            ..Default::default()
        };
        assert_eq!(collect_binary_names(&cc, "mytool"), vec!["mytool"]);
    }

    #[test]
    fn collect_binary_names_falls_back_when_builds_have_no_binary() {
        let cc = CrateConfig {
            builds: Some(vec![BuildConfig {
                binary: None,
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(collect_binary_names(&cc, "fallback"), vec!["fallback"]);
    }

    #[test]
    fn collect_binary_names_extracts_and_dedupes_preserving_order() {
        let cc = CrateConfig {
            builds: Some(vec![
                BuildConfig {
                    binary: Some("alpha".to_string()),
                    ..Default::default()
                },
                BuildConfig {
                    binary: Some("beta".to_string()),
                    ..Default::default()
                },
                BuildConfig {
                    binary: Some("alpha".to_string()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        assert_eq!(
            collect_binary_names(&cc, "ignored"),
            vec!["alpha".to_string(), "beta".to_string()]
        );
    }

    // -----------------------------------------------------------------
    // build_wrap_program_line — partitioned by `os:` filter.
    // -----------------------------------------------------------------

    #[test]
    fn build_wrap_program_line_returns_none_when_deps_empty() {
        assert!(build_wrap_program_line(&[], "mytool").is_none());
    }

    #[test]
    fn build_wrap_program_line_all_os_emits_unconditional_list() {
        let deps = vec![
            NixDependency {
                name: "git".to_string(),
                os: None,
            },
            NixDependency {
                name: "curl".to_string(),
                os: None,
            },
        ];
        let line = build_wrap_program_line(&deps, "mytool").expect("should emit");
        assert!(line.contains("wrapProgram $out/bin/mytool"));
        assert!(line.contains("[ git curl ]"));
        assert!(!line.contains("isDarwin"));
        assert!(!line.contains("isLinux"));
    }

    #[test]
    fn build_wrap_program_line_partitions_by_os() {
        let deps = vec![
            NixDependency {
                name: "darwin_dep".to_string(),
                os: Some("darwin".to_string()),
            },
            NixDependency {
                name: "linux_dep".to_string(),
                os: Some("linux".to_string()),
            },
            NixDependency {
                name: "git".to_string(),
                os: None,
            },
        ];
        let line = build_wrap_program_line(&deps, "mytool").expect("should emit");
        assert!(line.contains("lib.optionals stdenvNoCC.isDarwin [ darwin_dep ]"));
        assert!(line.contains("lib.optionals stdenvNoCC.isLinux [ linux_dep ]"));
        assert!(line.contains("[ git ]"));
        // Darwin must precede linux which must precede all-OS bucket.
        let darwin_pos = line.find("isDarwin").unwrap();
        let linux_pos = line.find("isLinux").unwrap();
        assert!(darwin_pos < linux_pos);
    }

    #[test]
    fn build_wrap_program_line_unknown_os_string_is_dropped() {
        let deps = vec![NixDependency {
            name: "freebsd_dep".to_string(),
            os: Some("freebsd".to_string()),
        }];
        assert!(build_wrap_program_line(&deps, "mytool").is_none());
    }

    // -----------------------------------------------------------------
    // build_install_lines — custom install vs auto-generated.
    // -----------------------------------------------------------------

    #[test]
    fn build_install_lines_custom_install_overrides_auto_block() {
        let nix_cfg = NixConfig {
            install: Some("custom-line-1\ncustom-line-2".to_string()),
            ..Default::default()
        };
        let cc = CrateConfig::default();
        let lines = build_install_lines(&nix_cfg, &cc, "mytool", &[], false);
        assert_eq!(lines, vec!["custom-line-1", "custom-line-2"]);
    }

    #[test]
    fn build_install_lines_custom_install_appends_extra_install() {
        let nix_cfg = NixConfig {
            install: Some("base".to_string()),
            extra_install: Some("extra-1\nextra-2".to_string()),
            ..Default::default()
        };
        let cc = CrateConfig::default();
        let lines = build_install_lines(&nix_cfg, &cc, "mytool", &[], false);
        assert_eq!(lines, vec!["base", "extra-1", "extra-2"]);
    }

    #[test]
    fn build_install_lines_auto_generates_mkdir_and_cp_per_binary() {
        let nix_cfg = NixConfig::default();
        let cc = CrateConfig {
            builds: Some(vec![BuildConfig {
                binary: Some("mytool".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let lines = build_install_lines(&nix_cfg, &cc, "mytool", &[], false);
        assert_eq!(lines[0], "mkdir -p $out/bin");
        assert!(lines.iter().any(|l| l == "cp -vr ./mytool $out/bin/mytool"));
        assert!(lines.iter().any(|l| l == "chmod +x $out/bin/mytool"));
    }

    #[test]
    fn build_install_lines_appends_wrap_program_when_needed() {
        let nix_cfg = NixConfig::default();
        let cc = CrateConfig::default();
        let deps = vec![NixDependency {
            name: "git".to_string(),
            os: None,
        }];
        let lines = build_install_lines(&nix_cfg, &cc, "mytool", &deps, true);
        let wrap = lines
            .iter()
            .find(|l| l.starts_with("wrapProgram"))
            .expect("wrap line must be appended");
        assert!(wrap.contains("[ git ]"));
    }

    #[test]
    fn build_install_lines_skips_wrap_program_when_deps_filter_to_empty() {
        // needs_make_wrapper=true but every dep is OS-filtered to an
        // unknown OS — build_wrap_program_line returns None, no wrap appended.
        let nix_cfg = NixConfig::default();
        let cc = CrateConfig::default();
        let deps = vec![NixDependency {
            name: "x".to_string(),
            os: Some("plan9".to_string()),
        }];
        let lines = build_install_lines(&nix_cfg, &cc, "mytool", &deps, true);
        assert!(!lines.iter().any(|l| l.starts_with("wrapProgram")));
    }

    // -----------------------------------------------------------------
    // build_archive_tuples — sha256 guard, url_template, hash conversion.
    // -----------------------------------------------------------------

    fn os_artifact(os: &str, arch: &str, url: &str, sha256: &str) -> util::OsArtifact {
        // Synthesize a representative genuine triple so `is_macos`-based nix
        // eligibility treats a "darwin" os as real macOS. Apple-but-not-macOS
        // targets (watchos/tvos) also map to os="darwin" but carry a different
        // triple — see `nix_system_for_artifact_excludes_apple_non_macos`.
        let target = match os {
            "darwin" => "aarch64-apple-darwin",
            "linux" => "x86_64-unknown-linux-gnu",
            "windows" => "x86_64-pc-windows-msvc",
            _ => "",
        };
        util::OsArtifact {
            url: url.to_string(),
            sha256: sha256.to_string(),
            os: os.to_string(),
            arch: arch.to_string(),
            target: target.to_string(),
            ..Default::default()
        }
    }

    /// Build an artifact carrying an explicit triple, so tests can drive the
    /// Apple-but-not-macOS eligibility path (`os` alone cannot express it).
    fn os_artifact_with_target(
        os: &str,
        arch: &str,
        target: &str,
        url: &str,
        sha256: &str,
    ) -> util::OsArtifact {
        util::OsArtifact {
            url: url.to_string(),
            sha256: sha256.to_string(),
            os: os.to_string(),
            arch: arch.to_string(),
            target: target.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn build_archive_tuples_empty_artifact_list_bails() {
        let cfg = NixConfig::default();
        let err =
            build_archive_tuples(&[], &cfg, "mytool", "1.0.0", &quiet_log()).expect_err("no arts");
        assert!(format!("{err}").contains("no Linux/Darwin archive"));
    }

    #[test]
    fn nix_system_for_artifact_excludes_apple_non_macos() {
        let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        // Genuine macOS (and Linux) stay nix-eligible.
        assert_eq!(
            nix_system_for_artifact(&os_artifact_with_target(
                "darwin",
                "arm64",
                "aarch64-apple-darwin",
                "u",
                sha,
            )),
            Some("aarch64-darwin".to_string()),
        );
        assert_eq!(
            nix_system_for_artifact(&os_artifact_with_target(
                "linux",
                "amd64",
                "x86_64-unknown-linux-gnu",
                "u",
                sha,
            )),
            Some("x86_64-linux".to_string()),
        );
        // map_target folds watchos/tvos into os="darwin"; these carry no
        // nix-installable binary and must NOT become a darwin nix system.
        for target in [
            "aarch64-apple-watchos",
            "aarch64-apple-tvos",
            "aarch64-apple-ios",
        ] {
            assert_eq!(
                nix_system_for_artifact(&os_artifact_with_target(
                    "darwin", "arm64", target, "u", sha,
                )),
                None,
                "{target} is Apple-but-not-macOS — must be nix-ineligible",
            );
        }
    }

    #[test]
    fn build_archive_tuples_excludes_watchos_darwin_keeps_linux() {
        // A watchOS archive maps to os="darwin" (map_target's broad apple rule)
        // but is not a real macOS binary; it must be dropped, leaving only the
        // genuine linux system in the tuples — never emitted as aarch64-darwin.
        let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let arts = vec![
            os_artifact_with_target(
                "darwin",
                "arm64",
                "aarch64-apple-watchos",
                "https://example.com/watch.tar.gz",
                sha,
            ),
            os_artifact_with_target(
                "linux",
                "amd64",
                "x86_64-unknown-linux-gnu",
                "https://example.com/linux.tar.gz",
                sha,
            ),
        ];
        let cfg = NixConfig::default();
        let tuples = build_archive_tuples(&arts, &cfg, "mytool", "1.0.0", &quiet_log()).unwrap();
        assert_eq!(tuples.len(), 1);
        assert_eq!(tuples[0].0, "x86_64-linux");
        assert!(
            !tuples.iter().any(|(sys, _, _)| sys.contains("darwin")),
            "watchOS archive must never surface as a darwin nix system"
        );
    }

    #[test]
    fn build_archive_tuples_only_apple_non_macos_bails_as_no_archive() {
        // A full build whose only Apple archive is tvOS has no nix-installable
        // system: build_archive_tuples must bail (failure surfaced), not emit a
        // bogus aarch64-darwin package.
        let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let arts = vec![os_artifact_with_target(
            "darwin",
            "arm64",
            "aarch64-apple-tvos",
            "https://example.com/tv.tar.gz",
            sha,
        )];
        let cfg = NixConfig::default();
        let err = build_archive_tuples(&arts, &cfg, "mytool", "1.0.0", &quiet_log())
            .expect_err("tvOS-only must bail");
        assert!(format!("{err}").contains("no Linux/Darwin archive"));
    }

    #[test]
    fn build_archive_tuples_missing_sha256_for_nix_system_bails() {
        let arts = vec![os_artifact(
            "linux",
            "amd64",
            "https://example.com/x.tar.gz",
            "",
        )];
        let cfg = NixConfig::default();
        let err = build_archive_tuples(&arts, &cfg, "mytool", "1.0.0", &quiet_log())
            .expect_err("empty sha256 must bail");
        let msg = format!("{err}");
        assert!(msg.contains("sha256"));
        assert!(msg.contains("mytool"));
    }

    #[test]
    fn build_archive_tuples_skips_non_nix_systems_silently() {
        // Windows artifact has no nix_system mapping; sha256-empty guard
        // should not trigger for it.
        let arts = vec![
            os_artifact("windows", "amd64", "https://example.com/x.zip", ""),
            os_artifact(
                "linux",
                "amd64",
                "https://example.com/x.tar.gz",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
        ];
        let cfg = NixConfig::default();
        let tuples = build_archive_tuples(&arts, &cfg, "mytool", "1.0.0", &quiet_log()).unwrap();
        assert_eq!(tuples.len(), 1);
        assert_eq!(tuples[0].0, "x86_64-linux");
    }

    #[test]
    fn build_archive_tuples_converts_hex_to_nix_base32() {
        let arts = vec![os_artifact(
            "linux",
            "amd64",
            "https://example.com/x.tar.gz",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )];
        let cfg = NixConfig::default();
        let tuples = build_archive_tuples(&arts, &cfg, "mytool", "1.0.0", &quiet_log()).unwrap();
        assert_eq!(tuples[0].2.len(), 52, "nix base32 must be 52 chars");
        assert_ne!(
            tuples[0].2, arts[0].sha256,
            "must convert, not pass hex through"
        );
    }

    #[test]
    fn build_archive_tuples_falls_back_to_raw_hex_on_bad_sha256() {
        // 64-char string that is NOT valid hex — base32 conversion fails,
        // warn-and-pass-through path runs (still yields a tuple).
        let bad = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";
        let arts = vec![os_artifact(
            "linux",
            "amd64",
            "https://example.com/x.tar.gz",
            bad,
        )];
        let cfg = NixConfig::default();
        let tuples = build_archive_tuples(&arts, &cfg, "mytool", "1.0.0", &quiet_log()).unwrap();
        assert_eq!(tuples[0].2, bad, "fallback must preserve raw hex");
    }

    #[test]
    fn build_archive_tuples_applies_url_template() {
        let arts = vec![os_artifact(
            "linux",
            "amd64",
            "https://original/url.tar.gz",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )];
        let cfg = NixConfig {
            url_template: Some(
                "https://mirror.example.com/{{ name }}-{{ version }}-{{ os }}-{{ arch }}.tar.gz"
                    .to_string(),
            ),
            ..Default::default()
        };
        let tuples = build_archive_tuples(&arts, &cfg, "mytool", "1.2.3", &quiet_log()).unwrap();
        assert_eq!(
            tuples[0].1,
            "https://mirror.example.com/mytool-1.2.3-linux-amd64.tar.gz"
        );
    }

    #[test]
    fn build_archive_tuples_dedupes_by_nix_system() {
        // Both an Archive and an UploadableBinary for the same target collapse
        // to one nix system (x86_64-linux). Without source dedup the pipeline
        // carries N tuples per system, triplicating meta.platforms AND emitting
        // an ambiguous urlMap/shaMap whose `selectSystem` winner is BTreeMap
        // last-writer-wins. Source dedup must keep exactly one tuple per system.
        let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let arts = vec![
            os_artifact("linux", "amd64", "https://example.com/a.tar.gz", sha),
            os_artifact("linux", "amd64", "https://example.com/a.bin", sha),
            os_artifact("linux", "amd64", "https://example.com/a2.tar.gz", sha),
            os_artifact("darwin", "arm64", "https://example.com/d.tar.gz", sha),
        ];
        let cfg = NixConfig::default();
        let tuples = build_archive_tuples(&arts, &cfg, "mytool", "1.0.0", &quiet_log()).unwrap();
        let systems: Vec<&str> = tuples.iter().map(|(s, _, _)| s.as_str()).collect();
        assert_eq!(
            systems,
            vec!["x86_64-linux", "aarch64-darwin"],
            "one tuple per nix system, first occurrence kept, insertion order preserved"
        );
        // First occurrence wins so the urlMap winner is the first-seen archive,
        // not a BTreeMap last-writer-wins surprise.
        assert_eq!(tuples[0].1, "https://example.com/a.tar.gz");
    }

    #[test]
    fn generate_nix_expression_emits_each_platform_once() {
        // Even if a caller somehow passes duplicate-system tuples, the rendered
        // meta.platforms must list each platform exactly once (deterministic,
        // sorted) — the historical bug rendered 12 entries for 4 platforms.
        let sha = "0bv1xkjqlf06hjyl3z7xj9zyq2k0q0k0q0k0q0k0q0k0q0k0q0k0";
        let archives = vec![
            (
                "x86_64-linux".to_string(),
                "https://e/a".to_string(),
                sha.to_string(),
            ),
            (
                "x86_64-linux".to_string(),
                "https://e/b".to_string(),
                sha.to_string(),
            ),
            (
                "x86_64-linux".to_string(),
                "https://e/c".to_string(),
                sha.to_string(),
            ),
            (
                "aarch64-darwin".to_string(),
                "https://e/d".to_string(),
                sha.to_string(),
            ),
        ];
        let expr = generate_nix_expression(&NixParams {
            name: "mytool",
            version: "1.0.0",
            description: "",
            long_description: "",
            homepage: "",
            changelog: "",
            license_expr: "",
            maintainers: &[],
            main_program: "",
            archives: &archives,
            install_lines: &[],
            post_install_lines: &[],
            needs_unzip: false,
            needs_make_wrapper: false,
            dep_args: &[],
            source_root: Some("."),
            source_root_map: None,
            dynamically_linked: false,
        })
        .unwrap();
        let platforms_line = expr
            .lines()
            .find(|l| l.trim_start().starts_with("platforms ="))
            .expect("platforms line present");
        assert_eq!(
            platforms_line.matches("\"x86_64-linux\"").count(),
            1,
            "x86_64-linux must appear exactly once in: {platforms_line}"
        );
        assert_eq!(
            platforms_line.matches("\"aarch64-darwin\"").count(),
            1,
            "aarch64-darwin must appear exactly once in: {platforms_line}"
        );
    }

    // -----------------------------------------------------------------
    // resolve_source_roots — single-root collapse vs per-system map.
    // -----------------------------------------------------------------

    #[test]
    fn resolve_source_roots_no_artifacts_yields_dot_default() {
        let cc = CrateConfig::default();
        let (single, map) = resolve_source_roots(&cc, &[], "mytool", "1.0.0");
        assert_eq!(single.as_deref(), Some("."));
        assert!(map.is_none());
    }

    #[test]
    fn resolve_source_roots_uniform_root_collapses_to_single() {
        let arts = vec![
            os_artifact("linux", "amd64", "u1", "h1"),
            os_artifact("darwin", "arm64", "u2", "h2"),
        ];
        let cc = CrateConfig {
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                wrap_in_directory: Some(WrapInDirectory::Bool(true)),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let (single, map) = resolve_source_roots(&cc, &arts, "mytool", "1.0.0");
        assert_eq!(single.as_deref(), Some("mytool-1.0.0"));
        assert!(map.is_none());
    }

    #[test]
    fn resolve_source_roots_disabled_archives_falls_back_to_dot() {
        let arts = vec![os_artifact("linux", "amd64", "u1", "h1")];
        let cc = CrateConfig {
            archives: ArchivesConfig::Disabled,
            ..Default::default()
        };
        let (single, map) = resolve_source_roots(&cc, &arts, "mytool", "1.0.0");
        assert_eq!(single.as_deref(), Some("."));
        assert!(map.is_none());
    }

    #[test]
    fn resolve_source_roots_divergent_per_id_emits_per_system_map() {
        let mut linux = os_artifact("linux", "amd64", "u1", "h1");
        linux.id = Some("linux-archive".to_string());
        let mut darwin = os_artifact("darwin", "arm64", "u2", "h2");
        darwin.id = Some("darwin-archive".to_string());
        let cc = CrateConfig {
            archives: ArchivesConfig::Configs(vec![
                ArchiveConfig {
                    id: Some("linux-archive".to_string()),
                    wrap_in_directory: Some(WrapInDirectory::Bool(true)),
                    ..Default::default()
                },
                ArchiveConfig {
                    id: Some("darwin-archive".to_string()),
                    wrap_in_directory: Some(WrapInDirectory::Bool(false)),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let (single, map) = resolve_source_roots(&cc, &[linux, darwin], "mytool", "1.0.0");
        assert!(single.is_none());
        let entries = map.expect("per-system map must be emitted");
        assert_eq!(entries.len(), 2);
        // Sorted by system identifier.
        assert!(entries[0].system < entries[1].system);
        let roots: std::collections::HashMap<&str, &str> = entries
            .iter()
            .map(|e| (e.system.as_str(), e.root.as_str()))
            .collect();
        assert_eq!(roots.get("x86_64-linux"), Some(&"mytool-1.0.0"));
        assert_eq!(roots.get("aarch64-darwin"), Some(&"."));
    }

    #[test]
    fn resolve_source_roots_single_unidentified_cfg_matches_id_bearing_artifact() {
        // The artifact carries an `id`, but the lone archive config has
        // `id: None`. The `(_, None) if archive_cfgs.len() == 1` fallback
        // matches it, so the custom wrap directory is applied to the system.
        let mut art = os_artifact("linux", "amd64", "u1", "h1");
        art.id = Some("some-archive-id".to_string());
        let cc = CrateConfig {
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                id: None,
                wrap_in_directory: Some(WrapInDirectory::Name("custom-root".to_string())),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let (single, map) = resolve_source_roots(&cc, &[art], "mytool", "1.0.0");
        assert_eq!(single.as_deref(), Some("custom-root"));
        assert!(map.is_none());
    }

    #[test]
    fn build_install_lines_auto_block_appends_extra_install() {
        // No custom `install`, so the auto mkdir/cp block runs; `extra_install`
        // must be appended after the generated cp/chmod lines.
        let nix_cfg = NixConfig {
            extra_install: Some("install -m644 LICENSE $out/share/LICENSE".to_string()),
            ..Default::default()
        };
        let cc = CrateConfig::default();
        let lines = build_install_lines(&nix_cfg, &cc, "mytool", &[], false);
        assert_eq!(lines[0], "mkdir -p $out/bin");
        assert!(lines.iter().any(|l| l == "cp -vr ./mytool $out/bin/mytool"));
        assert_eq!(
            lines.last().map(String::as_str),
            Some("install -m644 LICENSE $out/share/LICENSE"),
            "extra_install must be the final appended line on the auto path"
        );
    }

    // -----------------------------------------------------------------
    // detect_dynamically_linked — build-stage metadata flag short-circuit.
    // -----------------------------------------------------------------

    fn ctx_with_binary_metadata(crate_name: &str, flag: Option<&str>) -> Context {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::test_helpers::TestContextBuilder;
        let mut ctx = TestContextBuilder::new()
            .project_name("demo")
            .crates(vec![CrateConfig {
                name: crate_name.to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                ..Default::default()
            }])
            .build();
        let mut metadata = std::collections::HashMap::new();
        if let Some(v) = flag {
            metadata.insert("DynamicallyLinked".to_string(), v.to_string());
        }
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            // A path that does NOT exist on disk — proving the metadata flag
            // short-circuits before any ELF inspection of `path`.
            path: std::path::PathBuf::from("/nonexistent/anodizer-test-binary"),
            name: crate_name.to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: crate_name.to_string(),
            metadata,
            size: None,
        });
        ctx
    }

    #[test]
    fn detect_dynamically_linked_true_from_metadata_flag() {
        let ctx = ctx_with_binary_metadata("mytool", Some("true"));
        assert!(
            detect_dynamically_linked(&ctx, "mytool").unwrap(),
            "DynamicallyLinked=true metadata must report dynamic linkage \
             without touching the (nonexistent) binary path"
        );
    }

    #[test]
    fn detect_dynamically_linked_false_from_metadata_flag() {
        let ctx = ctx_with_binary_metadata("mytool", Some("false"));
        assert!(
            !detect_dynamically_linked(&ctx, "mytool").unwrap(),
            "DynamicallyLinked=false metadata must report static linkage \
             without falling through to ELF inspection of a missing path"
        );
    }

    // -----------------------------------------------------------------
    // resolve_nix_metadata — license resolution + meta.* render.
    // -----------------------------------------------------------------

    fn meta_ctx() -> Context {
        use anodizer_core::test_helpers::TestContextBuilder;
        TestContextBuilder::new()
            .project_name("demo")
            .crates(vec![CrateConfig {
                name: "mytool".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                ..Default::default()
            }])
            .build()
    }

    /// A bare crate config (no release repo, no archives) for the
    /// `resolve_nix_metadata` unit tests — they exercise description / homepage
    /// / license / main_program resolution, none of which need build artifacts.
    fn meta_crate_cfg() -> CrateConfig {
        CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_nix_metadata_resolves_spdx_license_to_nix_attr() {
        let ctx = meta_ctx();
        let cfg = NixConfig {
            description: Some("a demo".to_string()),
            homepage: Some("https://example.com".to_string()),
            license: Some("Apache-2.0".to_string()),
            main_program: Some("mytool".to_string()),
            ..Default::default()
        };
        let meta = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
            .expect("resolve");
        assert_eq!(meta.description, "a demo");
        assert_eq!(meta.homepage, "https://example.com");
        // SPDX `Apache-2.0` maps to the nix `lib.licenses.asl20` attribute.
        assert_eq!(meta.license_expr, "lib.licenses.asl20");
        assert_eq!(meta.main_program, "mytool");
    }

    #[test]
    fn resolve_nix_metadata_passes_through_raw_nix_license_attr() {
        let ctx = meta_ctx();
        let cfg = NixConfig {
            license: Some("mit".to_string()),
            ..Default::default()
        };
        let meta = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
            .expect("resolve");
        assert_eq!(
            meta.license_expr, "lib.licenses.mit",
            "a valid nix attr passes through verbatim"
        );
    }

    #[test]
    fn resolve_nix_metadata_empty_license_suppressed_not_resolved() {
        let ctx = meta_ctx();
        // No license configured and no project metadata fallback — the empty
        // value resolves to no `meta.license` attribute at all.
        let cfg = NixConfig::default();
        let meta = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
            .expect("resolve");
        assert_eq!(
            meta.license_expr, "",
            "empty license must stay empty, not error"
        );
        assert_eq!(meta.description, "");
        assert_eq!(meta.main_program, "");
    }

    #[test]
    fn resolve_nix_metadata_invalid_license_degrades_to_string() {
        let ctx = meta_ctx();
        let cfg = NixConfig {
            license: Some("not-a-real-license-xyz".to_string()),
            ..Default::default()
        };
        // An unmappable license no longer aborts the release; it degrades to
        // the verbatim quoted-string form (always valid in `meta`).
        let meta = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
            .expect("unmappable license must degrade, not bail");
        assert_eq!(meta.license_expr, "\"not-a-real-license-xyz\"");
    }

    #[test]
    fn resolve_nix_metadata_falls_back_to_project_metadata() {
        use anodizer_core::config::MetadataConfig;
        let mut ctx = meta_ctx();
        ctx.config.metadata = Some(MetadataConfig {
            description: Some("project-level description".to_string()),
            homepage: Some("https://project.example".to_string()),
            license: Some("MIT".to_string()),
            ..Default::default()
        });
        // NixConfig supplies none of these, so each must fall through to the
        // project `metadata.*` value (and the SPDX `MIT` resolves to nix `mit`).
        let cfg = NixConfig::default();
        let meta = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
            .expect("resolve");
        assert_eq!(meta.description, "project-level description");
        assert_eq!(meta.homepage, "https://project.example");
        assert_eq!(meta.license_expr, "lib.licenses.mit");
    }

    #[test]
    fn resolve_nix_metadata_config_overrides_project_metadata() {
        use anodizer_core::config::MetadataConfig;
        let mut ctx = meta_ctx();
        ctx.config.metadata = Some(MetadataConfig {
            description: Some("project-level".to_string()),
            homepage: Some("https://project.example".to_string()),
            license: Some("MIT".to_string()),
            ..Default::default()
        });
        let cfg = NixConfig {
            description: Some("nix-level".to_string()),
            homepage: Some("https://nix.example".to_string()),
            license: Some("Apache-2.0".to_string()),
            ..Default::default()
        };
        let meta = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
            .expect("resolve");
        assert_eq!(
            meta.description, "nix-level",
            "nix config wins over metadata"
        );
        assert_eq!(meta.homepage, "https://nix.example");
        assert_eq!(
            meta.license_expr, "lib.licenses.asl20",
            "Apache-2.0 resolves to asl20"
        );
    }

    #[test]
    fn resolve_nix_metadata_bad_homepage_template_bails() {
        let ctx = meta_ctx();
        let cfg = NixConfig {
            // Unterminated Tera expression — render must surface an Err that the
            // `with_context("render homepage template …")` wrapper carries up.
            homepage: Some("https://x/{{ unclosed".to_string()),
            ..Default::default()
        };
        let err = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
            .expect_err("malformed homepage template must bail");
        assert!(format!("{err:#}").contains("homepage"));
    }

    #[test]
    fn resolve_nix_metadata_bad_main_program_template_bails() {
        let ctx = meta_ctx();
        let cfg = NixConfig {
            main_program: Some("{{ unclosed".to_string()),
            ..Default::default()
        };
        let err = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
            .expect_err("malformed main_program template must bail");
        assert!(format!("{err:#}").contains("main_program"));
    }

    // -----------------------------------------------------------------
    // render_license_expr — RHS rendering for each NixLicense shape.
    // -----------------------------------------------------------------

    #[test]
    fn render_license_expr_single_attr() {
        assert_eq!(render_license_expr("MIT"), "lib.licenses.mit");
    }

    #[test]
    fn render_license_expr_dual_or_is_with_list() {
        assert_eq!(
            render_license_expr("MIT OR Apache-2.0"),
            "with lib.licenses; [ mit asl20 ]"
        );
    }

    #[test]
    fn render_license_expr_unknown_is_quoted_string() {
        assert_eq!(render_license_expr("Weird-9.9"), "\"Weird-9.9\"");
    }

    #[test]
    fn render_license_expr_compound_with_is_quoted_string() {
        assert_eq!(
            render_license_expr("Apache-2.0 WITH LLVM-exception"),
            "\"Apache-2.0 WITH LLVM-exception\""
        );
    }

    #[test]
    fn render_license_expr_empty_is_empty() {
        assert_eq!(render_license_expr(""), "");
    }

    // -----------------------------------------------------------------
    // resolve_nix_changelog — explicit override + release-repo derivation.
    // -----------------------------------------------------------------

    fn crate_cfg_with_github_release(owner: &str, repo: &str) -> CrateConfig {
        use anodizer_core::config::{ReleaseConfig, ScmRepoConfig};
        CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: owner.to_string(),
                    name: repo.to_string(),
                    token: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn changelog_derived_from_release_repo_and_tag() {
        let mut ctx = meta_ctx();
        ctx.template_vars_mut().set("Tag", "v1.4.2");
        let cc = crate_cfg_with_github_release("BurntSushi", "ripgrep");
        let cfg = NixConfig::default();
        let got = resolve_nix_changelog(&ctx, &cc, &cfg, &quiet_log()).expect("changelog");
        assert_eq!(
            got,
            "https://github.com/BurntSushi/ripgrep/releases/tag/v1.4.2"
        );
    }

    #[test]
    fn changelog_explicit_override_wins_and_templates() {
        let mut ctx = meta_ctx();
        ctx.template_vars_mut().set("Tag", "v1.4.2");
        let cc = crate_cfg_with_github_release("BurntSushi", "ripgrep");
        let cfg = NixConfig {
            changelog: Some(
                "https://github.com/BurntSushi/ripgrep/blob/{{ Tag }}/CHANGELOG.md".to_string(),
            ),
            ..Default::default()
        };
        let got = resolve_nix_changelog(&ctx, &cc, &cfg, &quiet_log()).expect("changelog");
        assert_eq!(
            got,
            "https://github.com/BurntSushi/ripgrep/blob/v1.4.2/CHANGELOG.md"
        );
    }

    #[test]
    fn changelog_empty_without_release_repo() {
        let ctx = meta_ctx();
        let cc = meta_crate_cfg();
        let cfg = NixConfig::default();
        let got = resolve_nix_changelog(&ctx, &cc, &cfg, &quiet_log()).expect("changelog");
        assert_eq!(
            got, "",
            "no release repo + no override → suppress changelog"
        );
    }

    #[test]
    fn changelog_falls_back_to_v_version_when_no_tag_var() {
        let mut ctx = meta_ctx();
        // Model an in-memory/snapshot render where no resolved git `Tag` exists
        // but a `Version` does — the URL falls back to `v<version>`.
        ctx.template_vars_mut().unset("Tag");
        ctx.template_vars_mut().set("Version", "2.0.0");
        let cc = crate_cfg_with_github_release("me", "tool");
        let cfg = NixConfig::default();
        let got = resolve_nix_changelog(&ctx, &cc, &cfg, &quiet_log()).expect("changelog");
        assert_eq!(got, "https://github.com/me/tool/releases/tag/v2.0.0");
    }

    // -----------------------------------------------------------------
    // build_completion_install_lines / build_manpage_install_lines —
    // gated on archive config; emit installShellCompletion / installManPage.
    // -----------------------------------------------------------------

    fn crate_cfg_with_archive(archive: anodizer_core::config::ArchiveConfig) -> CrateConfig {
        use anodizer_core::config::ArchivesConfig;
        CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            archives: ArchivesConfig::Configs(vec![archive]),
            ..Default::default()
        }
    }

    #[test]
    fn no_completion_lines_when_archive_has_no_completions() {
        let cc = crate_cfg_with_archive(anodizer_core::config::ArchiveConfig::default());
        assert!(build_completion_install_lines(&cc, &["mytool".to_string()]).is_empty());
        assert!(build_manpage_install_lines(&cc).is_empty());
    }

    #[test]
    fn completion_lines_emitted_when_archive_bundles_completions() {
        use anodizer_core::config::{ArchiveConfig, CompletionsConfig};
        let archive = ArchiveConfig {
            completions: Some(CompletionsConfig {
                generate: Some("{{ ArtifactPath }} completions {{ Shell }}".to_string()),
                shells: Some(vec![
                    "bash".to_string(),
                    "zsh".to_string(),
                    "fish".to_string(),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cc = crate_cfg_with_archive(archive);
        let lines = build_completion_install_lines(&cc, &["rg".to_string()]);
        assert_eq!(
            lines.len(),
            1,
            "one installShellCompletion line; got {lines:?}"
        );
        // clap filenames per shell under the default `completions/` dir.
        assert_eq!(
            lines[0],
            "installShellCompletion --cmd rg --bash completions/rg --zsh completions/_rg --fish completions/rg.fish"
        );
    }

    #[test]
    fn completion_lines_skip_shells_without_install_flag() {
        use anodizer_core::config::{ArchiveConfig, CompletionsConfig};
        // powershell/elvish have no `installShellCompletion` flag — they stay
        // bundled in the archive but are not install-flagged.
        let archive = ArchiveConfig {
            completions: Some(CompletionsConfig {
                generate: Some("x".to_string()),
                shells: Some(vec!["bash".to_string(), "powershell".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cc = crate_cfg_with_archive(archive);
        let lines = build_completion_install_lines(&cc, &["rg".to_string()]);
        assert_eq!(
            lines,
            vec!["installShellCompletion --cmd rg --bash completions/rg"]
        );
    }

    #[test]
    fn manpage_line_emitted_when_archive_bundles_manpages() {
        use anodizer_core::config::{ArchiveConfig, ManpagesConfig};
        let archive = ArchiveConfig {
            manpages: Some(ManpagesConfig {
                generate: Some("{{ ArtifactPath }} --man".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cc = crate_cfg_with_archive(archive);
        let lines = build_manpage_install_lines(&cc);
        assert_eq!(lines, vec!["installManPage man/man1/*"]);
    }

    // -----------------------------------------------------------------
    // resolve_repo_coords — owner/name resolution + render.
    // -----------------------------------------------------------------

    #[test]
    fn resolve_repo_coords_renders_owner_and_name_templates() {
        use anodizer_core::config::RepositoryConfig;
        let ctx = meta_ctx();
        let cfg = NixConfig {
            repository: Some(RepositoryConfig {
                owner: Some("acme-{{ ProjectName }}".to_string()),
                name: Some("nix-overlay".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let coords = resolve_repo_coords(&ctx, &cfg, "mytool", &quiet_log()).expect("coords");
        assert_eq!(
            coords.repo_owner, "acme-demo",
            "owner template must render {{ ProjectName }} -> demo"
        );
        assert_eq!(coords.repo_name, "nix-overlay");
    }

    #[test]
    fn resolve_repo_coords_missing_repository_bails() {
        let ctx = meta_ctx();
        let cfg = NixConfig::default();
        let err = resolve_repo_coords(&ctx, &cfg, "mytool", &quiet_log())
            .expect_err("absent repository config must bail");
        let msg = format!("{err}");
        assert!(msg.contains("no repository config"), "{msg}");
        assert!(msg.contains("mytool"), "{msg}");
    }

    // -----------------------------------------------------------------
    // render_nix_for_validation + crate_has_nix_archive — the in-memory
    // render twins (no clone, no subprocess). An Archive-kind artifact
    // never registers as a Binary, so detect_dynamically_linked finds no
    // binary artifacts and never touches disk — keeping these ungated.
    // -----------------------------------------------------------------

    fn archive_artifact(
        target: &str,
        url: &str,
        sha256: &str,
    ) -> anodizer_core::artifact::Artifact {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("url".to_string(), url.to_string());
        metadata.insert("sha256".to_string(), sha256.to_string());
        metadata.insert("format".to_string(), "tar.gz".to_string());
        Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("dist/{target}.tar.gz")),
            name: format!("mytool-{target}.tar.gz"),
            target: Some(target.to_string()),
            crate_name: "mytool".to_string(),
            metadata,
            size: None,
        }
    }

    fn validation_ctx(
        nix: NixConfig,
        artifacts: Vec<anodizer_core::artifact::Artifact>,
    ) -> Context {
        use anodizer_core::config::PublishConfig;
        use anodizer_core::test_helpers::TestContextBuilder;
        let mut ctx = TestContextBuilder::new()
            .project_name("demo")
            .crates(vec![CrateConfig {
                name: "mytool".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                publish: Some(PublishConfig {
                    nix: Some(nix),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        for a in artifacts {
            ctx.artifacts.add(a);
        }
        ctx
    }

    const VALID_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn render_nix_for_validation_renders_expression_without_clone() {
        let arts = vec![
            archive_artifact(
                "x86_64-unknown-linux-gnu",
                "https://e/x-linux.tar.gz",
                VALID_SHA,
            ),
            archive_artifact(
                "aarch64-apple-darwin",
                "https://e/x-darwin.tar.gz",
                VALID_SHA,
            ),
        ];
        let cfg = NixConfig {
            description: Some("demo tool".to_string()),
            ..Default::default()
        };
        let ctx = validation_ctx(cfg, arts);
        let render = render_nix_for_validation(&ctx, "mytool", &quiet_log())
            .expect("render ok")
            .expect("not skipped");
        assert_eq!(render.name, "mytool");
        assert!(
            render.expr.contains("pname = \"mytool\";"),
            "{}",
            render.expr
        );
        assert!(
            render.expr.contains("version = \"1.2.3\";"),
            "{}",
            render.expr
        );
        assert!(
            render.expr.contains("https://e/x-linux.tar.gz"),
            "linux archive url must be embedded: {}",
            render.expr
        );
        // Both systems mapped to a (system, url, hash) tuple.
        let systems: std::collections::HashSet<&str> =
            render.archives.iter().map(|(s, _, _)| s.as_str()).collect();
        assert!(systems.contains("x86_64-linux"));
        assert!(systems.contains("aarch64-darwin"));
    }

    #[test]
    fn render_nix_for_validation_returns_none_when_skipped() {
        use anodizer_core::config::StringOrBool;
        let arts = vec![archive_artifact(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            VALID_SHA,
        )];
        let cfg = NixConfig {
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };
        let ctx = validation_ctx(cfg, arts);
        let render = render_nix_for_validation(&ctx, "mytool", &quiet_log()).expect("ok");
        assert!(
            render.is_none(),
            "skip:true validation render must yield None"
        );
    }

    #[test]
    fn render_nix_for_validation_missing_nix_config_bails() {
        use anodizer_core::config::PublishConfig;
        use anodizer_core::test_helpers::TestContextBuilder;
        // Crate has a publish block but no `nix` publisher configured.
        let ctx = TestContextBuilder::new()
            .project_name("demo")
            .crates(vec![CrateConfig {
                name: "mytool".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                publish: Some(PublishConfig::default()),
                ..Default::default()
            }])
            .build();
        let err = render_nix_for_validation(&ctx, "mytool", &quiet_log())
            .expect_err("absent nix config must bail");
        assert!(format!("{err}").contains("no nix config"));
    }

    #[test]
    fn crate_has_nix_archive_true_when_nix_system_maps() {
        let arts = vec![archive_artifact(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            VALID_SHA,
        )];
        let cfg = NixConfig::default();
        let ctx = validation_ctx(cfg.clone(), arts);
        assert!(
            crate_has_nix_archive(&ctx, &cfg, "mytool").expect("ok"),
            "a linux archive maps to x86_64-linux"
        );
    }

    #[test]
    fn crate_has_nix_archive_false_when_only_non_nix_systems() {
        // A windows archive (valid sha256) maps to no nix system: genuine
        // absence, NOT an error — Ok(false), not Err.
        let arts = vec![archive_artifact(
            "x86_64-pc-windows-msvc",
            "https://e/x.zip",
            VALID_SHA,
        )];
        let cfg = NixConfig::default();
        let ctx = validation_ctx(cfg.clone(), arts);
        assert!(
            !crate_has_nix_archive(&ctx, &cfg, "mytool").expect("absence is Ok(false)"),
            "windows-only artifacts map to no nix system"
        );
    }

    #[test]
    fn crate_has_nix_archive_errors_on_present_but_sha_less_artifact() {
        // A matched artifact missing its sha256 is present-but-broken: the
        // collect step bails so the publisher surfaces it rather than skipping.
        let arts = vec![archive_artifact(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "",
        )];
        let cfg = NixConfig::default();
        let ctx = validation_ctx(cfg.clone(), arts);
        let err = crate_has_nix_archive(&ctx, &cfg, "mytool")
            .expect_err("missing sha256 on a present artifact must error, not skip");
        assert!(format!("{err}").contains("sha256"));
    }

    #[test]
    fn render_nix_for_validation_bails_on_sha_less_nix_artifact() {
        let arts = vec![archive_artifact(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            "",
        )];
        let cfg = NixConfig::default();
        let ctx = validation_ctx(cfg, arts);
        let err = render_nix_for_validation(&ctx, "mytool", &quiet_log())
            .expect_err("sha-less nix artifact must bail before rendering");
        assert!(format!("{err}").contains("sha256"));
    }

    #[test]
    fn render_nix_for_validation_bad_if_template_bails() {
        // A malformed `if` condition makes `check_skip_guards` -> the shared
        // `evaluate_if_condition` propagate an Err rather than a skip boolean.
        let arts = vec![archive_artifact(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            VALID_SHA,
        )];
        let cfg = NixConfig {
            if_condition: Some("{{ unclosed".to_string()),
            ..Default::default()
        };
        let ctx = validation_ctx(cfg, arts);
        let err = render_nix_for_validation(&ctx, "mytool", &quiet_log())
            .expect_err("malformed `if` template must propagate an error");
        assert!(format!("{err:#}").contains("nix publisher for crate 'mytool'"));
    }

    #[test]
    fn render_nix_for_validation_bad_skip_template_bails() {
        // A malformed `skip` template surfaces through the first guard's
        // `with_context("render skip template …")` wrapper.
        let arts = vec![archive_artifact(
            "x86_64-unknown-linux-gnu",
            "https://e/x.tar.gz",
            VALID_SHA,
        )];
        let cfg = NixConfig {
            skip: Some(anodizer_core::config::StringOrBool::String(
                "{{ unclosed".to_string(),
            )),
            ..Default::default()
        };
        let ctx = validation_ctx(cfg, arts);
        let err = render_nix_for_validation(&ctx, "mytool", &quiet_log())
            .expect_err("malformed `skip` template must propagate an error");
        assert!(format!("{err:#}").contains("skip template"));
    }

    // =================================================================
    // Subprocess-driven paths: formatter (alejandra/nixfmt) + the full
    // clone -> write -> flake -> commit -> push pipeline. Every test here
    // spawns `git` (and a fake formatter) and mutates `PATH`/env, so the
    // whole module is `#[cfg(unix)]`-gated (precedent: npm/tests.rs,
    // homebrew/publish_formula.rs). Coverage is measured on ubuntu, so the
    // gate costs nothing while keeping Windows builds warning-free.
    // =================================================================
    #[cfg(unix)]
    mod subprocess {
        use super::*;
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::{
            CommitAuthorConfig, GitRepoConfig, PublishConfig, ReleaseConfig, RepositoryConfig,
            StringOrBool,
        };
        use anodizer_core::context::Context;
        use anodizer_core::test_helpers::TestContextBuilder;
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        use serial_test::serial;
        use std::path::Path;
        use std::process::Command;

        const SAMPLE_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

        fn git_ok(dir: &Path, args: &[&str]) {
            anodizer_core::test_helpers::git_test_ok(dir, args)
        }

        fn git_stdout(dir: &Path, args: &[&str]) -> String {
            anodizer_core::test_helpers::git_test_stdout(dir, args)
        }

        /// Bare overlay repo seeded with one commit on `branch`, usable as a
        /// local `git clone` URL. The publisher clones it, writes the
        /// derivation + flake, commits, and pushes back — the bare repo is the
        /// assertion surface (inspect the landed `default.nix` / `flake.nix`).
        fn make_bare_repo(branch: &str) -> (String, tempfile::TempDir) {
            let bare = tempfile::tempdir().expect("bare tempdir");
            let seed = tempfile::tempdir().expect("seed tempdir");
            git_ok(bare.path(), &["init", "--bare", "-b", branch]);
            git_ok(seed.path(), &["init", "-b", branch]);
            git_ok(seed.path(), &["config", "user.email", "t@example.invalid"]);
            git_ok(seed.path(), &["config", "user.name", "T"]);
            git_ok(seed.path(), &["config", "commit.gpgsign", "false"]);
            std::fs::write(seed.path().join("README"), "overlay\n").unwrap();
            git_ok(seed.path(), &["add", "README"]);
            git_ok(seed.path(), &["commit", "-m", "seed overlay"]);
            assert!(
                anodizer_core::test_helpers::output_with_spawn_retry(
                    || {
                        let mut cmd = Command::new("git");
                        cmd.args(["remote", "add", "origin"])
                            .arg(bare.path())
                            .current_dir(seed.path());
                        cmd
                    },
                    "git",
                )
                .status
                .success(),
                "git remote add origin failed"
            );
            git_ok(seed.path(), &["push", "-u", "origin", branch]);
            (bare.path().to_string_lossy().into_owned(), bare)
        }

        /// Read a file's content as landed on the bare repo's `branch` ref.
        fn show(bare: &Path, branch: &str, path: &str) -> String {
            git_stdout(bare, &["show", &format!("{branch}:{path}")])
        }

        fn archive(target: &str, url: &str, sha: &str) -> Artifact {
            let mut metadata = std::collections::HashMap::new();
            metadata.insert("url".to_string(), url.to_string());
            metadata.insert("sha256".to_string(), sha.to_string());
            metadata.insert("format".to_string(), "tar.gz".to_string());
            Artifact {
                kind: ArtifactKind::Archive,
                path: std::path::PathBuf::from(format!("/tmp/{target}.tar.gz")),
                name: format!("mytool-{target}.tar.gz"),
                target: Some(target.to_string()),
                crate_name: "mytool".to_string(),
                metadata,
                size: None,
            }
        }

        fn nix_cfg_local(bare_url: &str, branch: &str) -> NixConfig {
            NixConfig {
                repository: Some(RepositoryConfig {
                    owner: Some("myorg".to_string()),
                    name: Some("nix-overlay".to_string()),
                    branch: Some(branch.to_string()),
                    git: Some(GitRepoConfig {
                        url: Some(bare_url.to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        fn ctx_for(nix: NixConfig, artifacts: Vec<Artifact>) -> Context {
            let mut ctx = TestContextBuilder::new()
                .crates(vec![CrateConfig {
                    name: "mytool".to_string(),
                    path: ".".to_string(),
                    tag_template: Some("v{{ .Version }}".to_string()),
                    release: Some(ReleaseConfig {
                        github: Some(anodizer_core::config::ScmRepoConfig {
                            owner: "myorg".to_string(),
                            name: "mytool".to_string(),
                            token: None,
                        }),
                        ..Default::default()
                    }),
                    publish: Some(PublishConfig {
                        nix: Some(nix),
                        ..Default::default()
                    }),
                    ..Default::default()
                }])
                .build();
            for a in artifacts {
                ctx.artifacts.add(a);
            }
            ctx
        }

        fn two_archives() -> Vec<Artifact> {
            vec![
                archive(
                    "x86_64-unknown-linux-gnu",
                    "https://e/mytool-linux-x64.tar.gz",
                    SAMPLE_SHA,
                ),
                archive(
                    "aarch64-apple-darwin",
                    "https://e/mytool-darwin-arm64.tar.gz",
                    SAMPLE_SHA,
                ),
            ]
        }

        // -------------------------------------------------------------
        // run_formatter — mandatory-format matrix. Formatting is opt-in
        // (None = no-op, matches GR) but once a formatter is configured it
        // is MANDATORY in EVERY mode (no --strict gate): a missing binary,
        // a non-zero exit, or an unknown name each bail so an unformatted
        // derivation is never pushed. INTENTIONALLY stricter than GR.
        // -------------------------------------------------------------

        #[test]
        fn run_formatter_none_is_noop() {
            let cfg = NixConfig::default();
            let tmp = tempfile::tempdir().unwrap();
            let f = tmp.path().join("default.nix");
            std::fs::write(&f, "{}\n").unwrap();
            run_formatter(&cfg, &f, &quiet_log()).expect("no formatter is Ok");
        }

        #[test]
        #[serial]
        fn run_formatter_runs_configured_alejandra_with_file_arg() {
            let tools = FakeToolDir::new();
            tools.tool("alejandra").install();
            let _path = tools.activate();
            let cfg = NixConfig {
                formatter: Some("alejandra".to_string()),
                ..Default::default()
            };
            let tmp = tempfile::tempdir().unwrap();
            let f = tmp.path().join("default.nix");
            std::fs::write(&f, "{}\n").unwrap();
            run_formatter(&cfg, &f, &quiet_log()).expect("formatter success is Ok");
            // The version-flag probe (`tool_detect::runs`) and the format run
            // each invoke the fake tool once.
            let calls = tools.calls("alejandra");
            assert_eq!(
                calls.last().expect("alejandra invoked"),
                &vec![f.to_string_lossy().to_string()],
                "formatter receives the generated file path as its sole arg"
            );
        }

        #[test]
        #[serial]
        fn run_formatter_nonzero_exit_bails_even_in_lenient_mode() {
            // No --strict set: the bail must fire regardless, so an
            // unformatted derivation is never pushed. The stub answers the
            // presence probe (`--version`) with exit 0 but fails the actual
            // format invocation with exit 3.
            let tools = FakeToolDir::new();
            tools
                .tool("nixfmt")
                .script(
                    "case \"$1\" in --version) exit 0 ;; *) echo 'parse error' 1>&2; exit 3 ;; esac",
                )
                .install();
            let _path = tools.activate();
            let cfg = NixConfig {
                formatter: Some("nixfmt".to_string()),
                ..Default::default()
            };
            let tmp = tempfile::tempdir().unwrap();
            let f = tmp.path().join("default.nix");
            std::fs::write(&f, "{}\n").unwrap();
            let err = run_formatter(&cfg, &f, &quiet_log())
                .expect_err("non-zero formatter exit must bail in lenient mode too");
            let msg = format!("{err}");
            assert!(msg.contains("nixfmt formatting failed"), "{msg}");
            assert!(
                msg.contains("refusing to push an unformatted derivation"),
                "{msg}"
            );
            assert!(msg.contains("exit 3"), "{msg}");
        }

        #[test]
        #[serial]
        fn run_formatter_missing_binary_bails_with_install_remedy() {
            // A FakeToolDir that installs NO formatter: `alejandra` is a
            // recognized name but absent from PATH, so the presence probe
            // fails and run_formatter bails (no --strict needed). Prepending
            // (rather than emptying) PATH keeps git/sh available for other
            // concurrently-running tests and avoids a process-wide PATH race.
            let tools = FakeToolDir::new();
            let _path = tools.activate();
            let cfg = NixConfig {
                formatter: Some("alejandra".to_string()),
                ..Default::default()
            };
            let tmp = tempfile::tempdir().unwrap();
            let f = tmp.path().join("default.nix");
            std::fs::write(&f, "{}\n").unwrap();
            let err = run_formatter(&cfg, &f, &quiet_log())
                .expect_err("missing formatter binary must bail in lenient mode");
            let msg = format!("{err}");
            assert!(
                msg.contains("formatter 'alejandra' not found on PATH"),
                "{msg}"
            );
            assert!(msg.contains("install it"), "{msg}");
        }

        #[test]
        fn run_formatter_unknown_name_bails_in_lenient_mode() {
            let cfg = NixConfig {
                formatter: Some("rustfmt".to_string()),
                ..Default::default()
            };
            let tmp = tempfile::tempdir().unwrap();
            let f = tmp.path().join("default.nix");
            std::fs::write(&f, "{}\n").unwrap();
            let err = run_formatter(&cfg, &f, &quiet_log())
                .expect_err("unrecognized formatter must bail in lenient mode");
            let msg = format!("{err}");
            assert!(msg.contains("unknown formatter 'rustfmt'"), "{msg}");
            assert!(msg.contains("alejandra or nixfmt"), "{msg}");
        }

        // -------------------------------------------------------------
        // publish_to_nix — full clone/write/flake/commit/push pipeline.
        // -------------------------------------------------------------

        #[test]
        fn publish_to_nix_direct_push_lands_derivation_and_flake() {
            let (bare_url, bare) = make_bare_repo("main");
            let nix = nix_cfg_local(&bare_url, "main");
            let mut ctx = ctx_for(nix, two_archives());

            let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
            assert!(pushed, "a real push must return Ok(true)");

            let bare_path = Path::new(&bare_url);
            // Default path is pkgs/<name>/default.nix.
            let drv = show(bare_path, "main", "pkgs/mytool/default.nix");
            assert!(drv.contains("pname = \"mytool\";"), "{drv}");
            assert!(drv.contains("version = \"1.2.3\";"), "{drv}");
            assert!(
                drv.contains("https://e/mytool-linux-x64.tar.gz"),
                "linux archive url must be embedded: {drv}"
            );
            assert!(
                drv.contains("x86_64-linux") && drv.contains("aarch64-darwin"),
                "both nix systems must be mapped: {drv}"
            );
            // The root flake referencing the package is written too.
            let flake = show(bare_path, "main", "flake.nix");
            assert!(
                flake.contains("mytool"),
                "flake must reference package: {flake}"
            );

            let subject = git_stdout(bare_path, &["log", "-1", "--pretty=%s", "main"]);
            assert!(
                subject.contains("mytool") && subject.contains("1.2.3"),
                "commit subject must name package + version; got: {subject}"
            );
            drop(bare);
        }

        #[test]
        #[serial]
        fn publish_to_nix_formatter_absent_errors_and_pushes_nothing() {
            // A configured formatter that is absent from PATH must abort the
            // crate's nix publish BEFORE flake write / commit / push — nothing
            // lands on the overlay branch. The FakeToolDir installs NO
            // alejandra (prepend, not empty, so the file-URL clone's git still
            // resolves), so the presence probe fails after clone+write.
            let (bare_url, bare) = make_bare_repo("main");
            let mut nix = nix_cfg_local(&bare_url, "main");
            nix.formatter = Some("alejandra".to_string());
            let mut ctx = ctx_for(nix, two_archives());

            let bare_path = Path::new(&bare_url);
            let before = git_stdout(bare_path, &["rev-parse", "main"]);

            let tools = FakeToolDir::new();
            let _path = tools.activate();
            let res = publish_to_nix(&mut ctx, "mytool", &quiet_log());
            drop(_path);

            let err = res.expect_err("missing formatter must abort the publish");
            assert!(format!("{err}").contains("not found on PATH"), "{err}");
            // The overlay branch is untouched: same tip, no default.nix landed.
            let after = git_stdout(bare_path, &["rev-parse", "main"]);
            assert_eq!(before, after, "no commit must reach the overlay branch");
            let drv_present = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["cat-file", "-e", "main:pkgs/mytool/default.nix"])
                        .current_dir(bare_path);
                    cmd
                },
                "git",
            )
            .status
            .success();
            assert!(!drv_present, "no unformatted derivation must be pushed");
            drop(bare);
        }

        #[test]
        fn publish_to_nix_honors_custom_path_and_commit_author() {
            let (bare_url, bare) = make_bare_repo("main");
            let mut nix = nix_cfg_local(&bare_url, "main");
            nix.path = Some("packages/mytool.nix".to_string());
            nix.commit_author = Some(CommitAuthorConfig {
                name: Some("Nix Bot".to_string()),
                email: Some("nix-bot@example.invalid".to_string()),
                ..Default::default()
            });
            let mut ctx = ctx_for(nix, two_archives());

            let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
            assert!(pushed);

            let bare_path = Path::new(&bare_url);
            let drv = show(bare_path, "main", "packages/mytool.nix");
            assert!(drv.contains("pname = \"mytool\";"), "{drv}");
            // The default pkgs/<name>/default.nix path must NOT exist.
            let default_path = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["cat-file", "-e", "main:pkgs/mytool/default.nix"])
                        .current_dir(bare_path);
                    cmd
                },
                "git",
            )
            .status;
            assert!(
                !default_path.success(),
                "derivation must live at the configured path, not the default"
            );
            // The configured commit_author must drive the landed author —
            // proving the identity is applied via the GIT_AUTHOR_* child env
            // (which overrides inherited env + repo config), not via
            // `-c user.name=` (which git's precedence defeats whenever an
            // ambient GIT_AUTHOR_NAME is present).
            let author = git_stdout(bare_path, &["log", "-1", "--pretty=%an", "main"]);
            assert_eq!(author, "Nix Bot", "configured commit author must drive %an");
            let author_email = git_stdout(bare_path, &["log", "-1", "--pretty=%ae", "main"]);
            assert_eq!(
                author_email, "nix-bot@example.invalid",
                "configured commit author email must drive %ae over the ambient GIT_AUTHOR_EMAIL"
            );
            drop(bare);
        }

        #[test]
        fn publish_to_nix_second_run_is_noop_no_extra_commit() {
            let (bare_url, bare) = make_bare_repo("main");
            let nix = nix_cfg_local(&bare_url, "main");
            let mut ctx = ctx_for(nix.clone(), two_archives());
            publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("first publish");
            let bare_path = Path::new(&bare_url);
            let head1 = git_stdout(bare_path, &["rev-parse", "main"]);

            let mut ctx2 = ctx_for(nix, two_archives());
            let pushed2 =
                publish_to_nix(&mut ctx2, "mytool", &quiet_log()).expect("second publish");
            let head2 = git_stdout(bare_path, &["rev-parse", "main"]);
            assert!(!pushed2, "an unchanged re-publish must report no push");
            assert_eq!(head1, head2, "no new commit when nothing changed");
            drop(bare);
        }

        #[test]
        fn publish_to_nix_dry_run_makes_no_commit() {
            let (bare_url, bare) = make_bare_repo("main");
            let nix = nix_cfg_local(&bare_url, "main");
            let mut ctx = ctx_for(nix, two_archives());
            ctx.options.dry_run = true;
            let bare_path = Path::new(&bare_url);
            let head_before = git_stdout(bare_path, &["rev-parse", "main"]);

            let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("dry-run ok");
            assert!(!pushed, "dry-run must not push");
            let head_after = git_stdout(bare_path, &["rev-parse", "main"]);
            assert_eq!(
                head_before, head_after,
                "dry-run must leave the repo untouched"
            );
            drop(bare);
        }

        #[test]
        fn publish_to_nix_skip_true_returns_false_without_clone() {
            // `skip: true` short-circuits before any repo coordinate is even
            // resolved; an invalid bare URL would error if a clone were
            // attempted, so a clean Ok(false) proves the skip gate fired first.
            let mut nix = nix_cfg_local("/nonexistent/not-a-repo", "main");
            nix.skip = Some(StringOrBool::Bool(true));
            let mut ctx = ctx_for(nix, two_archives());
            let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("skip ok");
            assert!(!pushed, "skip:true must return Ok(false) and not clone");
        }

        #[test]
        fn publish_to_nix_if_condition_falsy_returns_false_without_clone() {
            let mut nix = nix_cfg_local("/nonexistent/not-a-repo", "main");
            nix.if_condition = Some("false".to_string());
            let mut ctx = ctx_for(nix, two_archives());
            let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("if-falsy ok");
            assert!(!pushed, "falsy `if` must return Ok(false) and not clone");
        }

        #[test]
        fn publish_to_nix_skip_upload_returns_false_without_clone() {
            let mut nix = nix_cfg_local("/nonexistent/not-a-repo", "main");
            nix.skip_upload = Some(StringOrBool::Bool(true));
            let mut ctx = ctx_for(nix, two_archives());
            let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("skip_upload ok");
            assert!(!pushed, "skip_upload must return Ok(false) and not clone");
        }

        #[test]
        fn publish_to_nix_pull_request_enabled_records_outcome() {
            // With `pull_request.enabled = true`, finalize_publish drives
            // maybe_submit_pr, which yields Some(outcome) and is recorded on
            // the context. The direct push still lands; the PR attempt (no gh
            // resolvable against a fake fork) surfaces a recorded outcome —
            // proving the `if let Some(pr_outcome)` branch ran (a non-PR
            // publish records nothing).
            use anodizer_core::config::PullRequestConfig;
            let (bare_url, bare) = make_bare_repo("main");
            let mut nix = nix_cfg_local(&bare_url, "main");
            if let Some(repo) = nix.repository.as_mut() {
                repo.pull_request = Some(PullRequestConfig {
                    enabled: Some(true),
                    ..Default::default()
                });
            }
            let mut ctx = ctx_for(nix, two_archives());
            let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
            assert!(pushed, "the direct push to the overlay branch still lands");
            assert!(
                ctx.take_pending_outcome().is_some(),
                "an enabled pull_request must record a publisher outcome"
            );
            // The landed derivation is still correct.
            let drv = show(Path::new(&bare_url), "main", "pkgs/mytool/default.nix");
            assert!(drv.contains("pname = \"mytool\";"), "{drv}");
            drop(bare);
        }

        #[test]
        #[serial]
        fn publish_to_nix_runs_configured_formatter_on_generated_file() {
            // A configured formatter is invoked against the written derivation
            // before commit; the fake formatter records its argv so we can
            // assert the generated default.nix path was handed to it.
            let tools = FakeToolDir::new();
            tools.tool("nixfmt").install();
            let _path = tools.activate();
            let (bare_url, bare) = make_bare_repo("main");
            let mut nix = nix_cfg_local(&bare_url, "main");
            nix.formatter = Some("nixfmt".to_string());
            let mut ctx = ctx_for(nix, two_archives());
            let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
            assert!(pushed);
            // run_formatter probes presence (`--version`) then formats; assert
            // the generated default.nix path was handed to the format call.
            let calls = tools.calls("nixfmt");
            let formatted = calls.iter().any(|c| {
                c.last()
                    .is_some_and(|p| p.ends_with("pkgs/mytool/default.nix"))
            });
            assert!(
                formatted,
                "formatter must receive the generated derivation path: {calls:?}"
            );
            drop(bare);
        }

        #[test]
        fn publish_to_nix_embeds_post_install_and_custom_install_lines() {
            // Exercises the install_lines / post_install_lines plumbing of
            // render_nix_derivation_inner end-to-end: both land verbatim in
            // the rendered derivation.
            let (bare_url, bare) = make_bare_repo("main");
            let mut nix = nix_cfg_local(&bare_url, "main");
            nix.install = Some("mkdir -p $out/bin\ncp ./mytool $out/bin/".to_string());
            nix.post_install = Some("echo done >$out/.installed".to_string());
            let mut ctx = ctx_for(nix, two_archives());
            publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
            let drv = show(Path::new(&bare_url), "main", "pkgs/mytool/default.nix");
            assert!(
                drv.contains("cp ./mytool $out/bin/"),
                "custom install line must be embedded: {drv}"
            );
            assert!(
                drv.contains("echo done >$out/.installed"),
                "post_install line must be embedded: {drv}"
            );
            drop(bare);
        }

        /// A `nix.description` template that fails to render (undefined
        /// field) falls back to its raw `{{ }}` text via `render_or_warn` and
        /// lands in the derivation — `guard_no_unrendered` must hard-fail the
        /// real publish before anything is written to the overlay branch.
        #[test]
        fn publish_residual_description_template_errors_before_push() {
            let (bare_url, bare) = make_bare_repo("main");
            let mut nix = nix_cfg_local(&bare_url, "main");
            nix.description = Some("{{ .NoSuchField }}".to_string());
            let mut ctx = ctx_for(nix, two_archives());

            let bare_path = Path::new(&bare_url);
            let before = git_stdout(bare_path, &["rev-parse", "main"]);

            let err = publish_to_nix(&mut ctx, "mytool", &quiet_log())
                .expect_err("residual {{ }} in the derivation must hard-fail");
            assert!(
                format!("{err:#}").contains("nix derivation"),
                "error must name the manifest label; got: {err:#}"
            );
            let after = git_stdout(bare_path, &["rev-parse", "main"]);
            assert_eq!(
                before, after,
                "a residual-delimiter bail must leave the overlay branch untouched"
            );
            drop(bare);
        }

        /// The same residual `nix.description` template stays lenient in
        /// dry-run: `publish_to_nix` early-returns before the derivation
        /// render (and therefore before the guard), so the call must still
        /// report `Ok(false)` rather than surface the residual as an error.
        #[test]
        fn publish_residual_description_template_dry_run_stays_lenient() {
            let (bare_url, bare) = make_bare_repo("main");
            let mut nix = nix_cfg_local(&bare_url, "main");
            nix.description = Some("{{ .NoSuchField }}".to_string());
            let mut ctx = TestContextBuilder::new()
                .crates(vec![CrateConfig {
                    name: "mytool".to_string(),
                    path: ".".to_string(),
                    tag_template: Some("v{{ .Version }}".to_string()),
                    release: Some(ReleaseConfig {
                        github: Some(anodizer_core::config::ScmRepoConfig {
                            owner: "myorg".to_string(),
                            name: "mytool".to_string(),
                            token: None,
                        }),
                        ..Default::default()
                    }),
                    publish: Some(PublishConfig {
                        nix: Some(nix),
                        ..Default::default()
                    }),
                    ..Default::default()
                }])
                .dry_run(true)
                .build();
            for a in two_archives() {
                ctx.artifacts.add(a);
            }

            let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log())
                .expect("dry-run must stay lenient on a residual template");
            assert!(!pushed, "dry-run must report no push");
            drop(bare);
        }
    }
}
