//! `publish_to_nix` orchestrator — resolves config, gathers artifacts,
//! generates the Nix expression, and pushes it to the configured repo.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util;

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
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "nix")?;

    let nix_cfg = publish
        .nix
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("nix: no nix config for '{}'", crate_name))?;

    // Honor `skip` first (template-aware), then `skip_upload`.
    if let Some(d) = nix_cfg.skip.as_ref() {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("nix: render skip template for '{}'", crate_name))?;
        if off {
            log.status(&format!("nix: config skipped for '{}'", crate_name));
            return Ok(false);
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
        return Ok(false);
    }

    let (repo_owner_raw, repo_name_raw) =
        crate::util::resolve_repo_owner_name(nix_cfg.repository.as_ref())
            .ok_or_else(|| anyhow::anyhow!("nix: no repository config for '{}'", crate_name))?;
    let repo_owner = ctx
        .render_template(&repo_owner_raw)
        .unwrap_or(repo_owner_raw);
    let repo_name = ctx.render_template(&repo_name_raw).unwrap_or(repo_name_raw);

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
    // GoReleaser Pro parity: fall back to project `metadata.*` when nix config unset.
    // The `description`/`homepage`/`license`/`main_program` Tera variables
    // below are each gated by `{% if X %}...{% endif %}` blocks in
    // NIX_TEMPLATE — an empty string suppresses the corresponding
    // `meta.<field>` attribute, which is valid under the Nix `meta` schema
    // (every attribute under `meta` is optional per nixpkgs convention).
    let description_raw = nix_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description())
        .unwrap_or("");
    let description_rendered = ctx
        .render_template(description_raw)
        .unwrap_or_else(|_| description_raw.to_string());
    let description = description_rendered.as_str();
    let homepage_raw = nix_cfg
        .homepage
        .as_deref()
        .or_else(|| ctx.config.meta_homepage())
        .unwrap_or("");
    let homepage_rendered = ctx
        .render_template(homepage_raw)
        .with_context(|| format!("nix: render homepage template for '{}'", crate_name))?;
    let homepage = homepage_rendered.as_str();
    let license = nix_cfg
        .license
        .as_deref()
        .or_else(|| ctx.config.meta_license())
        .unwrap_or("");

    // Validate license identifier against known Nix licenses (skip if empty).
    if !license.is_empty() {
        validate_nix_license(license)?;
    }

    let main_program_raw = nix_cfg.main_program.as_deref().unwrap_or("");
    let main_program_rendered = ctx
        .render_template(main_program_raw)
        .with_context(|| format!("nix: render main_program template for '{}'", crate_name))?;
    let main_program = main_program_rendered.as_str();

    // Find artifacts for Linux and Darwin platforms, applying IDs + amd64_variant filter.
    let ids_filter = nix_cfg.ids.as_deref();
    let amd64_variant = nix_cfg.amd64_variant.as_deref().or(Some("v1"));
    let all_artifacts = util::find_all_platform_artifacts_with_variant(
        ctx,
        crate_name,
        ids_filter,
        amd64_variant,
        None,
    );

    let url_template = nix_cfg.url_template.as_deref();

    // The Nix derivation's `fetchurl { sha256 = ...; }` is a content-addressed
    // fixed-output derivation — `nix-build` refuses to evaluate when the hash
    // attribute is empty or a placeholder, and downstream consumers cannot
    // install a derivation whose source hash fails to verify. Bail before
    // emitting a broken expression rather than letting the empty default ship.
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
    let archives: Vec<(String, String, String)> = all_artifacts
        .iter()
        .filter_map(|a| {
            let system = nix_system(&a.os, &a.arch)?;
            let download_url = if let Some(tmpl) = url_template {
                util::render_url_template(tmpl, crate_name, &version, &a.arch, &a.os)
            } else {
                a.url.clone()
            };
            // convert hex SHA256 to nix-native base32 format
            // (the same output as `nix-hash --type sha256 --flat --base32`).
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

    // Check if any archive is a zip (needs unzip dep)
    let needs_unzip = all_artifacts.iter().any(|a| a.url.ends_with(".zip"));

    // Check if dependencies are configured (needs makeWrapper)
    let deps = nix_cfg.dependencies.as_deref().unwrap_or(&[]);
    let needs_make_wrapper = !deps.is_empty();

    // Collect unique dependency package names for the derivation function arguments.
    let dep_args: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        deps.iter()
            .filter(|d| seen.insert(d.name.clone()))
            .map(|d| d.name.clone())
            .collect()
    };

    // Build install lines
    let install_lines: Vec<String> = if let Some(ref custom_install) = nix_cfg.install {
        let mut lines: Vec<String> = custom_install.lines().map(|l| l.to_string()).collect();
        if let Some(ref extra) = nix_cfg.extra_install {
            lines.extend(extra.lines().map(|l| l.to_string()));
        }
        lines
    } else {
        let mut lines = vec!["mkdir -p $out/bin".to_string()];
        // install ALL binaries from the archive, not just
        // the package name.  Collect binary names from build configs; fall back
        // to the crate/derivation name when no builds are configured.
        let bin_names: Vec<String> = {
            let mut names: Vec<String> = _crate_cfg
                .builds
                .as_ref()
                .map(|builds| {
                    builds
                        .iter()
                        .filter_map(|b| b.binary.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            // Deduplicate while preserving order.
            let mut seen = std::collections::HashSet::new();
            names.retain(|n| seen.insert(n.clone()));
            if names.is_empty() {
                names.push(name.to_string());
            }
            names
        };
        for bin in &bin_names {
            lines.push(format!("cp -vr ./{bin} $out/bin/{bin}"));
            lines.push(format!("chmod +x $out/bin/{bin}"));
        }
        if let Some(ref extra) = nix_cfg.extra_install {
            lines.extend(extra.lines().map(|l| l.to_string()));
        }
        // Generate wrapProgram invocations from dependencies with OS filtering.
        if needs_make_wrapper {
            // Partition deps by OS for conditional wrapping.
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

            // Build lib.makeBinPath argument list with optional platform guards.
            let mut list_parts: Vec<String> = Vec::new();
            if !darwin_deps.is_empty() {
                let items = darwin_deps
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                list_parts.push(format!("lib.optionals stdenvNoCC.isDarwin [ {items} ]"));
            }
            if !linux_deps.is_empty() {
                let items = linux_deps
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                list_parts.push(format!("lib.optionals stdenvNoCC.isLinux [ {items} ]"));
            }
            if !all_os_deps.is_empty() {
                let items = all_os_deps
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                list_parts.push(format!("[ {items} ]"));
            }

            if !list_parts.is_empty() {
                let joined = list_parts.join(" ++\n      ");
                lines.push(format!(
                    "wrapProgram $out/bin/{name} --prefix PATH : ${{lib.makeBinPath (\n      {joined}\n    )}}"
                ));
            }
        }
        lines
    };

    let post_install_lines: Vec<String> = nix_cfg
        .post_install
        .as_ref()
        .map(|s| s.lines().map(|l| l.to_string()).collect())
        .unwrap_or_default();

    // Determine sourceRoot from the archive config's wrap_in_directory setting.
    // When an archive wraps contents in a directory, Nix needs to know the
    // extraction root.  We use a placeholder default name since the exact
    // archive stem is not available here; the template in wrap_in_directory
    // is typically a string like "myapp-1.0.0".
    //
    // GoReleaser supports per-platform sourceRoots when different archive
    // configs have different `WrappedIn` values.  We build a per-system map
    // and collapse to a single value when all are the same.
    let (source_root, source_root_map) = {
        let default_stem = format!("{}-{}", name, version);
        let archive_cfgs = match &_crate_cfg.archives {
            anodizer_core::config::ArchivesConfig::Configs(cfgs) => cfgs.clone(),
            anodizer_core::config::ArchivesConfig::Disabled => vec![],
        };

        // Build a map: nix_system -> sourceRoot by matching artifact IDs to
        // archive config IDs.
        let mut per_system: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for art in &all_artifacts {
            if let Some(system) = nix_system(&art.os, &art.arch) {
                // Find the archive config that produced this artifact.
                let wrap_dir = archive_cfgs
                    .iter()
                    .find(|cfg| {
                        // Match by ID when both the artifact and config have one.
                        match (&art.id, &cfg.id) {
                            (Some(aid), Some(cid)) => aid == cid,
                            // When there's only one archive config with no ID,
                            // it applies to all artifacts.
                            (_, None) if archive_cfgs.len() == 1 => true,
                            _ => false,
                        }
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

        // Check if all values are the same.
        let unique_roots: std::collections::HashSet<&str> =
            per_system.values().map(|s| s.as_str()).collect();

        if unique_roots.len() <= 1 {
            // All the same (or empty): use a single source_root.
            let single = per_system
                .values()
                .next()
                .cloned()
                .unwrap_or_else(|| ".".to_string());
            (Some(single), None)
        } else {
            // Different per platform: build source_root_map entries.
            let mut entries: Vec<SourceRootEntry> = per_system
                .into_iter()
                .map(|(system, root)| SourceRootEntry { system, root })
                .collect();
            entries.sort_by(|a, b| a.system.cmp(&b.system));
            (None, Some(entries))
        }
    };

    // Detect dynamically-linked binaries (GoReleaser parity: check artifact
    // metadata `dynamically_linked` set by the build stage first, then fall
    // back to inspecting actual binary files on disk for non-build artifacts).
    let dynamically_linked = {
        let binary_artifacts = ctx
            .artifacts
            .by_kind_and_crate(anodizer_core::artifact::ArtifactKind::Binary, crate_name);
        binary_artifacts.iter().any(|a| {
            // Check metadata first (set by build stage, avoids redundant disk I/O)
            if let Some(v) = a.metadata.get("DynamicallyLinked") {
                return v == "true";
            }
            // Fall back to direct ELF inspection
            is_dynamically_linked(&a.path)
        })
    };

    let nix_expr = generate_nix_expression(&NixParams {
        name,
        version: &version,
        description,
        homepage,
        license,
        main_program,
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

    // Optionally format with alejandra or nixfmt
    // (only if the formatter binary is available)

    // Clone repo (SSH-aware), write nix expression, commit, push.
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

    // Write nix file at configured path or default
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

    // Run formatter if configured
    if let Some(ref formatter) = nix_cfg.formatter {
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
    }

    log.status(&format!("wrote Nix expression: {}", nix_file.display()));

    let previous_tag = ctx
        .template_vars()
        .get("PreviousTag")
        .cloned()
        .unwrap_or_default();
    let commit_msg = crate::homebrew::render_commit_msg_with_prev(
        nix_cfg.commit_msg_template.as_deref(),
        name,
        &version,
        &previous_tag,
        "nix",
    );
    let commit_opts = util::resolve_commit_opts(ctx, nix_cfg.commit_author.as_ref());
    let branch = util::resolve_branch(nix_cfg.repository.as_ref());
    let outcome = util::commit_and_push_with_opts(
        repo_path,
        &[&nix_path],
        &commit_msg,
        branch,
        "nix",
        &commit_opts,
    )?;

    // Submit PR if configured.
    // Clone the repository config so the `maybe_submit_pr` call no
    // longer borrows from `ctx.config` (via `nix_cfg`). NLL then
    // drops the immutable borrow, making the subsequent `&mut ctx`
    // call legal.
    let repo_for_pr = nix_cfg.repository.clone();
    let pr_branch = branch.unwrap_or("main").to_string();
    let pr_outcome = util::maybe_submit_pr(
        repo_path,
        repo_for_pr.as_ref(),
        &util::PrOrigin {
            repo_owner: &repo_owner,
            repo_name: &repo_name,
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

    // Surface PR-already-exists skips to the dispatch summary table.
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
