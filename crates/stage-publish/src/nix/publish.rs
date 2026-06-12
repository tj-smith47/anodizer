//! `publish_to_nix` orchestrator — resolves config, gathers artifacts,
//! generates the Nix expression, and pushes it to the configured repo.

use std::path::Path;

use anodizer_core::config::{NixConfig, NixDependency};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util::{self, OsArtifact};

use super::binary::is_dynamically_linked;
use super::generate::{NixParams, SourceRootEntry, generate_nix_expression, nix_system};
use super::hashing::hex_sha256_to_nix_base32;
use super::resolve_nix_license;

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

    run_formatter(ctx, nix_cfg, &nix_file, log)?;

    log.status(&format!("wrote Nix expression: {}", nix_file.display()));

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
        "wrote root flake: {}",
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
        .any(|a| nix_system(&a.os, &a.arch).is_some()))
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
    let meta = resolve_nix_metadata(ctx, nix_cfg, crate_name, log)?;

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

    let dynamically_linked = detect_dynamically_linked(ctx, crate_name);

    let expr = generate_nix_expression(&NixParams {
        name: &name,
        version: &version,
        description: meta.description.as_str(),
        homepage: meta.homepage.as_str(),
        license: meta.license.as_str(),
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
    homepage: String,
    license: String,
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
            log.status(&format!("skipped nix config for '{}'", crate_name));
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
            "skipping nix for '{}' — `if` condition evaluated falsy",
            crate_name
        ));
        return Ok(true);
    }
    if util::should_skip_upload(nix_cfg.skip_upload.as_ref(), ctx, log)? {
        log.status(&format!(
            "skipping nix upload for '{}' (skip_upload={})",
            crate_name,
            nix_cfg
                .skip_upload
                .as_ref()
                .map(|v| v.as_str())
                .unwrap_or("")
        ));
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

    let homepage_raw = nix_cfg
        .homepage
        .as_deref()
        .or_else(|| ctx.config.meta_homepage_for(crate_name))
        .unwrap_or("");
    let homepage = ctx
        .render_template(homepage_raw)
        .with_context(|| format!("nix: render homepage template for '{}'", crate_name))?;

    // The raw value can be a nix `lib.licenses` attribute (config-supplied,
    // a direct nix attr) OR an SPDX id derived from `Cargo.toml`
    // `[package].license`. Resolve to a nix attribute so both paths emit a
    // valid `lib.licenses.<attr>`; an empty value suppresses `meta.license`.
    let license_raw = nix_cfg
        .license
        .as_deref()
        .or_else(|| ctx.config.meta_license_for(crate_name))
        .unwrap_or("");
    let license = if license_raw.is_empty() {
        String::new()
    } else {
        resolve_nix_license(license_raw)?
    };

    let main_program_raw = nix_cfg.main_program.as_deref().unwrap_or("");
    let main_program = ctx
        .render_template(main_program_raw)
        .with_context(|| format!("nix: render main_program template for '{}'", crate_name))?;

    Ok(NixMetadata {
        description,
        homepage,
        license,
        main_program,
    })
}

// ---------------------------------------------------------------------------
// Artifact + archive helpers
// ---------------------------------------------------------------------------

/// Gathers all Linux/Darwin platform artifacts for the crate, applying
/// the configured ID filter and `amd64_variant` (defaulting to `v1`).
fn collect_platform_artifacts(
    ctx: &Context,
    crate_name: &str,
    nix_cfg: &NixConfig,
) -> anyhow::Result<Vec<OsArtifact>> {
    let ids_filter = nix_cfg.ids.as_deref();
    let amd64_variant = nix_cfg.amd64_variant.as_deref().or(Some("v1"));
    util::find_all_platform_artifacts_with_variant(ctx, crate_name, ids_filter, amd64_variant, None)
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
        .find(|a| nix_system(&a.os, &a.arch).is_some() && a.sha256.is_empty())
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
    let archives: Vec<(String, String, String)> = all_artifacts
        .iter()
        .filter_map(|a| {
            let system = nix_system(&a.os, &a.arch)?;
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
    if let Some(ref extra) = nix_cfg.extra_install {
        lines.extend(extra.lines().map(|l| l.to_string()));
    }
    if needs_make_wrapper && let Some(wrap_line) = build_wrap_program_line(deps, name) {
        lines.push(wrap_line);
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
        if let Some(system) = nix_system(&art.os, &art.arch) {
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
fn detect_dynamically_linked(ctx: &Context, crate_name: &str) -> bool {
    let binary_artifacts = ctx
        .artifacts
        .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Binary, crate_name);
    binary_artifacts.iter().any(|a| {
        if let Some(v) = a.metadata.get("DynamicallyLinked") {
            return v == "true";
        }
        is_dynamically_linked(&a.path)
    })
}

// ---------------------------------------------------------------------------
// Formatter + commit/push helpers
// ---------------------------------------------------------------------------

/// Runs the optional `alejandra` / `nixfmt` formatter against the
/// generated file. Strict-mode guards fire on a non-zero exit, a
/// missing binary, or an unrecognized formatter name.
fn run_formatter(
    ctx: &mut Context,
    nix_cfg: &NixConfig,
    nix_file: &Path,
    log: &StageLogger,
) -> Result<()> {
    let Some(ref formatter) = nix_cfg.formatter else {
        return Ok(());
    };
    let nix_file_str = nix_file.to_string_lossy();
    match formatter.as_str() {
        "alejandra" | "nixfmt" => {
            if let Ok(output) = std::process::Command::new(formatter)
                .arg(&*nix_file_str)
                .output()
            {
                if !output.status.success() {
                    ctx.strict_guard(log, &format!("nix: {} formatting failed", formatter))?;
                }
            } else {
                ctx.strict_guard(
                    log,
                    &format!("nix: {} not available, skipping format", formatter),
                )?;
            }
        }
        _ => {
            ctx.strict_guard(
                log,
                &format!("nix: unknown formatter '{}', skipping", formatter),
            )?;
        }
    }
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
    let branch = util::resolve_branch(ctx, nix_cfg.repository.as_ref());
    let outcome = util::commit_and_push_with_opts(
        repo_path,
        files,
        &commit_msg,
        branch.as_deref(),
        "nix",
        &commit_opts,
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
            "## Package\n- **Name**: {}\n- **Version**: {}\n\nAutomatically submitted by anodizer.",
            name, version
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
        util::OsArtifact {
            url: url.to_string(),
            sha256: sha256.to_string(),
            os: os.to_string(),
            arch: arch.to_string(),
            id: None,
            amd64_variant: None,
            arm_variant: None,
            binary: None,
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
                tag_template: "v{{ .Version }}".to_string(),
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
            detect_dynamically_linked(&ctx, "mytool"),
            "DynamicallyLinked=true metadata must report dynamic linkage \
             without touching the (nonexistent) binary path"
        );
    }

    #[test]
    fn detect_dynamically_linked_false_from_metadata_flag() {
        let ctx = ctx_with_binary_metadata("mytool", Some("false"));
        assert!(
            !detect_dynamically_linked(&ctx, "mytool"),
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
                tag_template: "v{{ .Version }}".to_string(),
                ..Default::default()
            }])
            .build()
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
        let meta = resolve_nix_metadata(&ctx, &cfg, "mytool", &quiet_log()).expect("resolve");
        assert_eq!(meta.description, "a demo");
        assert_eq!(meta.homepage, "https://example.com");
        // SPDX `Apache-2.0` maps to the nix `lib.licenses.asl20` attribute.
        assert_eq!(meta.license, "asl20");
        assert_eq!(meta.main_program, "mytool");
    }

    #[test]
    fn resolve_nix_metadata_passes_through_raw_nix_license_attr() {
        let ctx = meta_ctx();
        let cfg = NixConfig {
            license: Some("mit".to_string()),
            ..Default::default()
        };
        let meta = resolve_nix_metadata(&ctx, &cfg, "mytool", &quiet_log()).expect("resolve");
        assert_eq!(
            meta.license, "mit",
            "a valid nix attr passes through verbatim"
        );
    }

    #[test]
    fn resolve_nix_metadata_empty_license_suppressed_not_resolved() {
        let ctx = meta_ctx();
        // No license configured and no project metadata fallback — the empty
        // sentinel must short-circuit BEFORE resolve_nix_license (which would
        // bail on an empty string).
        let cfg = NixConfig::default();
        let meta = resolve_nix_metadata(&ctx, &cfg, "mytool", &quiet_log()).expect("resolve");
        assert_eq!(meta.license, "", "empty license must stay empty, not error");
        assert_eq!(meta.description, "");
        assert_eq!(meta.main_program, "");
    }

    #[test]
    fn resolve_nix_metadata_invalid_license_bails() {
        let ctx = meta_ctx();
        let cfg = NixConfig {
            license: Some("not-a-real-license-xyz".to_string()),
            ..Default::default()
        };
        let err = resolve_nix_metadata(&ctx, &cfg, "mytool", &quiet_log())
            .expect_err("unknown license must bail");
        assert!(format!("{err}").contains("not-a-real-license-xyz"));
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
        let meta = resolve_nix_metadata(&ctx, &cfg, "mytool", &quiet_log()).expect("resolve");
        assert_eq!(meta.description, "project-level description");
        assert_eq!(meta.homepage, "https://project.example");
        assert_eq!(meta.license, "mit");
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
        let meta = resolve_nix_metadata(&ctx, &cfg, "mytool", &quiet_log()).expect("resolve");
        assert_eq!(
            meta.description, "nix-level",
            "nix config wins over metadata"
        );
        assert_eq!(meta.homepage, "https://nix.example");
        assert_eq!(meta.license, "asl20", "Apache-2.0 resolves to asl20");
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
        let err = resolve_nix_metadata(&ctx, &cfg, "mytool", &quiet_log())
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
        let err = resolve_nix_metadata(&ctx, &cfg, "mytool", &quiet_log())
            .expect_err("malformed main_program template must bail");
        assert!(format!("{err:#}").contains("main_program"));
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
                tag_template: "v{{ .Version }}".to_string(),
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
                tag_template: "v{{ .Version }}".to_string(),
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
        use std::sync::OnceLock;

        const SAMPLE_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

        fn git_ok(dir: &Path, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
            assert!(status.success(), "git {args:?} failed");
        }

        fn git_stdout(dir: &Path, args: &[&str]) -> String {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
            assert!(out.status.success(), "git {args:?} failed");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        fn ensure_git_identity() {
            static INIT: OnceLock<()> = OnceLock::new();
            INIT.get_or_init(|| {
                // SAFETY: runs exactly once per process, guarded by OnceLock;
                // values are constants, not user input.
                unsafe {
                    std::env::set_var("GIT_AUTHOR_NAME", "Anodize Test");
                    std::env::set_var("GIT_AUTHOR_EMAIL", "test@anodize.local");
                    std::env::set_var("GIT_COMMITTER_NAME", "Anodize Test");
                    std::env::set_var("GIT_COMMITTER_EMAIL", "test@anodize.local");
                    std::env::set_var("GIT_TERMINAL_PROMPT", "0");
                }
            });
        }

        /// Bare overlay repo seeded with one commit on `branch`, usable as a
        /// local `git clone` URL. The publisher clones it, writes the
        /// derivation + flake, commits, and pushes back — the bare repo is the
        /// assertion surface (inspect the landed `default.nix` / `flake.nix`).
        fn make_bare_repo(branch: &str) -> (String, tempfile::TempDir) {
            ensure_git_identity();
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
                Command::new("git")
                    .args(["remote", "add", "origin"])
                    .arg(bare.path())
                    .current_dir(seed.path())
                    .status()
                    .expect("git remote add origin")
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
                    tag_template: "v{{ .Version }}".to_string(),
                    release: Some(ReleaseConfig {
                        github: Some(anodizer_core::config::ScmRepoConfig {
                            owner: "myorg".to_string(),
                            name: "mytool".to_string(),
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
        // run_formatter — strict-guard matrix (no formatter, unknown,
        // success, non-zero exit, missing binary).
        // -------------------------------------------------------------

        fn lenient_ctx() -> Context {
            TestContextBuilder::new().project_name("demo").build()
        }

        #[test]
        fn run_formatter_none_is_noop() {
            let mut ctx = lenient_ctx();
            let cfg = NixConfig::default();
            let tmp = tempfile::tempdir().unwrap();
            let f = tmp.path().join("default.nix");
            std::fs::write(&f, "{}\n").unwrap();
            run_formatter(&mut ctx, &cfg, &f, &quiet_log()).expect("no formatter is Ok");
        }

        #[test]
        #[serial]
        fn run_formatter_runs_configured_alejandra_with_file_arg() {
            let tools = FakeToolDir::new();
            tools.tool("alejandra").install();
            let _path = tools.activate();
            let mut ctx = lenient_ctx();
            let cfg = NixConfig {
                formatter: Some("alejandra".to_string()),
                ..Default::default()
            };
            let tmp = tempfile::tempdir().unwrap();
            let f = tmp.path().join("default.nix");
            std::fs::write(&f, "{}\n").unwrap();
            run_formatter(&mut ctx, &cfg, &f, &quiet_log()).expect("formatter success is Ok");
            let calls = tools.calls("alejandra");
            assert_eq!(calls.len(), 1, "alejandra invoked exactly once");
            assert_eq!(
                calls[0],
                vec![f.to_string_lossy().to_string()],
                "formatter receives the generated file path as its sole arg"
            );
        }

        #[test]
        #[serial]
        fn run_formatter_nonzero_exit_strict_bails() {
            let tools = FakeToolDir::new();
            tools.tool("nixfmt").exit(3).install();
            let _path = tools.activate();
            let mut ctx = lenient_ctx();
            ctx.options.strict = true;
            let cfg = NixConfig {
                formatter: Some("nixfmt".to_string()),
                ..Default::default()
            };
            let tmp = tempfile::tempdir().unwrap();
            let f = tmp.path().join("default.nix");
            std::fs::write(&f, "{}\n").unwrap();
            let err = run_formatter(&mut ctx, &cfg, &f, &quiet_log())
                .expect_err("non-zero formatter exit must bail under strict");
            let msg = format!("{err}");
            assert!(msg.contains("nixfmt formatting failed"), "{msg}");
            assert!(msg.contains("strict mode"), "{msg}");
        }

        #[test]
        #[serial]
        fn run_formatter_nonzero_exit_lenient_warns_and_continues() {
            let tools = FakeToolDir::new();
            tools.tool("nixfmt").exit(3).install();
            let _path = tools.activate();
            let mut ctx = lenient_ctx();
            let cfg = NixConfig {
                formatter: Some("nixfmt".to_string()),
                ..Default::default()
            };
            let tmp = tempfile::tempdir().unwrap();
            let f = tmp.path().join("default.nix");
            std::fs::write(&f, "{}\n").unwrap();
            run_formatter(&mut ctx, &cfg, &f, &quiet_log())
                .expect("lenient mode warns but returns Ok on formatter failure");
        }

        #[test]
        fn run_formatter_missing_binary_strict_bails() {
            // Use an absolute empty PATH-free dir so the named formatter cannot
            // be found; the spawn `Err` routes through strict_guard.
            let mut ctx = lenient_ctx();
            ctx.options.strict = true;
            let cfg = NixConfig {
                formatter: Some("alejandra-definitely-not-installed-xyz".to_string()),
                ..Default::default()
            };
            let tmp = tempfile::tempdir().unwrap();
            let f = tmp.path().join("default.nix");
            std::fs::write(&f, "{}\n").unwrap();
            let err = run_formatter(&mut ctx, &cfg, &f, &quiet_log())
                .expect_err("unknown formatter name must bail under strict");
            assert!(format!("{err}").contains("skipping"));
        }

        #[test]
        fn run_formatter_unknown_name_strict_bails() {
            let mut ctx = lenient_ctx();
            ctx.options.strict = true;
            let cfg = NixConfig {
                formatter: Some("rustfmt".to_string()),
                ..Default::default()
            };
            let tmp = tempfile::tempdir().unwrap();
            let f = tmp.path().join("default.nix");
            std::fs::write(&f, "{}\n").unwrap();
            let err = run_formatter(&mut ctx, &cfg, &f, &quiet_log())
                .expect_err("unrecognized formatter must bail under strict");
            assert!(format!("{err}").contains("unknown formatter 'rustfmt'"));
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
            let default_path = Command::new("git")
                .args(["cat-file", "-e", "main:pkgs/mytool/default.nix"])
                .current_dir(bare_path)
                .status()
                .expect("git cat-file");
            assert!(
                !default_path.success(),
                "derivation must live at the configured path, not the default"
            );
            // The configured commit_author must win over the ambient
            // GIT_AUTHOR_NAME/EMAIL that ensure_git_identity() exports into the
            // process env — proving the identity is applied via the
            // GIT_AUTHOR_* child env (which overrides inherited env + repo
            // config), not via `-c user.name=` (which git's precedence defeats
            // whenever an ambient GIT_AUTHOR_NAME is present).
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
            let calls = tools.calls("nixfmt");
            assert_eq!(calls.len(), 1, "formatter invoked exactly once in-pipeline");
            assert!(
                calls[0]
                    .last()
                    .is_some_and(|p| p.ends_with("pkgs/mytool/default.nix")),
                "formatter must receive the generated derivation path: {:?}",
                calls[0]
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
    }
}
