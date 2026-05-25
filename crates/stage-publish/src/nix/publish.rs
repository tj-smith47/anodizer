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
use super::validate_nix_license;

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

    if let Some(reason) = check_skip_guards(ctx, nix_cfg, crate_name, log)? {
        let _ = reason;
        return Ok(false);
    }

    let RepoCoords {
        repo_owner,
        repo_name,
    } = resolve_repo_coords(ctx, nix_cfg, crate_name)?;

    let name_raw = nix_cfg.name.as_deref().unwrap_or(crate_name);
    let name_rendered = ctx
        .render_template(name_raw)
        .unwrap_or_else(|_| name_raw.to_string());
    let name = name_rendered.as_str();

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would publish Nix expression for '{}' to {}/{}",
            crate_name, repo_owner, repo_name
        ));
        return Ok(false);
    }

    let version = ctx.version();
    let meta = resolve_nix_metadata(ctx, nix_cfg, crate_name)?;

    let all_artifacts = collect_platform_artifacts(ctx, crate_name, nix_cfg);
    let archives = build_archive_tuples(&all_artifacts, nix_cfg, crate_name, &version, log)?;

    let needs_unzip = all_artifacts.iter().any(|a| a.url.ends_with(".zip"));
    let deps = nix_cfg.dependencies.as_deref().unwrap_or(&[]);
    let needs_make_wrapper = !deps.is_empty();
    let dep_args = unique_dep_args(deps);

    let install_lines = build_install_lines(nix_cfg, crate_cfg, name, deps, needs_make_wrapper);
    let post_install_lines: Vec<String> = nix_cfg
        .post_install
        .as_ref()
        .map(|s| s.lines().map(|l| l.to_string()).collect())
        .unwrap_or_default();

    let (source_root, source_root_map) =
        resolve_source_roots(crate_cfg, &all_artifacts, name, &version);

    let dynamically_linked = detect_dynamically_linked(ctx, crate_name);

    let nix_expr = generate_nix_expression(&NixParams {
        name,
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

    let token = util::resolve_repo_token(ctx, nix_cfg.repository.as_ref(), Some("NIX_PKGS_TOKEN"));

    let tmp_dir = tempfile::tempdir().context("nix: create temp dir")?;
    let repo_path = tmp_dir.path();
    util::clone_repo(
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

    finalize_publish(
        ctx,
        nix_cfg,
        repo_path,
        &nix_path,
        name,
        &version,
        &repo_owner,
        &repo_name,
        crate_name,
        log,
    )
}

// ---------------------------------------------------------------------------
// Skip / repo / metadata helpers
// ---------------------------------------------------------------------------

/// Carrier for the two repo coordinates after template rendering.
struct RepoCoords {
    repo_owner: String,
    repo_name: String,
}

/// Bundle of rendered `meta.*` strings ready to feed into `NixParams`.
struct NixMetadata {
    description: String,
    homepage: String,
    license: String,
    main_program: String,
}

/// Inspects `skip` / `skip_upload` and returns `Some(reason)` when the
/// publish must short-circuit. Emits the same log lines the inline
/// version emitted, preserving observable behavior.
fn check_skip_guards(
    ctx: &mut Context,
    nix_cfg: &NixConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<&'static str>> {
    if let Some(d) = nix_cfg.skip.as_ref() {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("nix: render skip template for '{}'", crate_name))?;
        if off {
            log.status(&format!("nix: config skipped for '{}'", crate_name));
            return Ok(Some("skip"));
        }
    }
    if util::should_skip_upload(nix_cfg.skip_upload.as_ref(), ctx, log) {
        log.status(&format!(
            "nix: skipping upload for '{}' (skip_upload={})",
            crate_name,
            nix_cfg
                .skip_upload
                .as_ref()
                .map(|v| v.as_str())
                .unwrap_or("")
        ));
        return Ok(Some("skip_upload"));
    }
    Ok(None)
}

/// Resolves `(owner, name)` from the repository config and renders both
/// halves through the template engine.
fn resolve_repo_coords(
    ctx: &mut Context,
    nix_cfg: &NixConfig,
    crate_name: &str,
) -> Result<RepoCoords> {
    let (repo_owner_raw, repo_name_raw) =
        crate::util::resolve_repo_owner_name(nix_cfg.repository.as_ref())
            .ok_or_else(|| anyhow::anyhow!("nix: no repository config for '{}'", crate_name))?;
    let repo_owner = ctx
        .render_template(&repo_owner_raw)
        .unwrap_or(repo_owner_raw);
    let repo_name = ctx.render_template(&repo_name_raw).unwrap_or(repo_name_raw);
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
    ctx: &mut Context,
    nix_cfg: &NixConfig,
    crate_name: &str,
) -> Result<NixMetadata> {
    let description_raw = nix_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description())
        .unwrap_or("");
    let description = ctx
        .render_template(description_raw)
        .unwrap_or_else(|_| description_raw.to_string());

    let homepage_raw = nix_cfg
        .homepage
        .as_deref()
        .or_else(|| ctx.config.meta_homepage())
        .unwrap_or("");
    let homepage = ctx
        .render_template(homepage_raw)
        .with_context(|| format!("nix: render homepage template for '{}'", crate_name))?;

    let license = nix_cfg
        .license
        .as_deref()
        .or_else(|| ctx.config.meta_license())
        .unwrap_or("")
        .to_string();
    if !license.is_empty() {
        validate_nix_license(&license)?;
    }

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
) -> Vec<OsArtifact> {
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
                        "nix: failed to convert SHA256 to nix base32 for {}: {}; using raw hex",
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
    nix_path: &str,
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
    );
    let commit_opts = util::resolve_commit_opts(ctx, nix_cfg.commit_author.as_ref());
    let branch = util::resolve_branch(nix_cfg.repository.as_ref());
    let outcome = util::commit_and_push_with_opts(
        repo_path,
        &[nix_path],
        &commit_msg,
        branch,
        "nix",
        &commit_opts,
    )?;

    // Clone the repository config so `maybe_submit_pr` no longer
    // borrows from `ctx.config` (via `nix_cfg`). NLL then drops the
    // immutable borrow, making the subsequent `&mut ctx` call legal.
    let repo_for_pr = nix_cfg.repository.clone();
    let pr_branch = branch.unwrap_or("main").to_string();
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
                "nix: nothing to push, expression for '{}' already up to date",
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

    #[test]
    fn commit_outcome_is_pushed() {
        assert!(util::CommitOutcome::Pushed.is_pushed());
        assert!(!util::CommitOutcome::NoChanges.is_pushed());
    }
}
