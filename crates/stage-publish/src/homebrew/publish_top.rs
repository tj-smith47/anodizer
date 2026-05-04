//! `publish_top_level_homebrew_casks` — emits cask `.rb` files from the
//! top-level `homebrew_casks:` config block (independent of any per-crate
//! homebrew config).
use super::cask::{CaskParams, find_top_level_cask_artifact, generate_cask};
use super::commit_msg::render_commit_msg;
use super::formula::{
    build_conflicts_directives, build_depends_directives, build_uninstall_directives,
};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};
pub fn publish_top_level_homebrew_casks(ctx: &Context, log: &StageLogger) -> Result<()> {
    let entries = match ctx.config.homebrew_casks {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(()),
    };

    for cask_cfg in entries {
        let project_name = &ctx.config.project_name;
        let cask_name = cask_cfg.name.as_deref().unwrap_or(project_name);
        let version = ctx.version();

        // Check skip_upload.
        if crate::util::should_skip_upload(cask_cfg.skip_upload.as_ref(), ctx, log) {
            log.status(&format!(
                "homebrew_casks: skipping upload for '{}' (skip_upload)",
                cask_name
            ));
            continue;
        }

        // Repository is required for top-level cask.
        let repo_cfg = cask_cfg.repository.as_ref();
        let (repo_owner, repo_name) =
            crate::util::resolve_repo_owner_name(repo_cfg).ok_or_else(|| {
                anyhow::anyhow!(
                    "homebrew_casks: no repository config for cask '{}'",
                    cask_name
                )
            })?;

        // Directory defaults to "Casks" (mirrors GR cask.go:65-67). GR warns
        // when the resolved value is not "Casks" since a non-default cask
        // directory typically breaks `brew install` on end-user machines
        // (homebrew-cask only auto-discovers files under "Casks/"). Pin
        // C-new-10: emit the same warning here.
        let directory_raw = cask_cfg.directory.as_deref().unwrap_or("Casks");
        let directory = ctx
            .render_template(directory_raw)
            .unwrap_or_else(|_| directory_raw.to_string());
        if directory != "Casks" {
            log.warn(&format!(
                "homebrew_casks: directory {:?} might not work properly for end users; \
                 the homebrew-cask convention is \"Casks\"",
                directory
            ));
        }

        if ctx.is_dry_run() {
            log.status(&format!(
                "(dry-run) would update Homebrew cask '{}/{}' in {}/{}/{}",
                repo_owner, repo_name, repo_owner, repo_name, directory
            ));
            continue;
        }

        // Find macOS artifact: prefer DiskImage, then Archive with darwin target.
        // For top-level cask, iterate all crates' artifacts.
        let macos_artifact = find_top_level_cask_artifact(ctx, cask_cfg.ids.as_deref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "homebrew_casks: no macOS artifact (DiskImage or Archive) found for cask '{}'",
                    cask_name
                )
            })?;

        // Build URL.
        let url = if let Some(ref url_cfg) = cask_cfg.url {
            if let Some(ref tmpl) = url_cfg.template {
                let target = macos_artifact.target.as_deref().unwrap_or("");
                let (os, arch) = anodizer_core::target::map_target(target);
                crate::util::render_url_template(tmpl, macos_artifact.name(), &version, &arch, &os)
            } else {
                macos_artifact
                    .metadata
                    .get("url")
                    .cloned()
                    .unwrap_or_default()
            }
        } else {
            macos_artifact.metadata.get("url").cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "homebrew_casks: artifact for '{}' has no 'url' metadata; set url.template",
                    cask_name
                )
            })?
        };

        // replace version string with #{version} for auto-update
        let url = url.replace(&version, "#{version}");

        let sha256 = macos_artifact
            .metadata
            .get("sha256")
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "homebrew_casks: artifact has no 'sha256' metadata for cask '{}'",
                    cask_name
                )
            })?;

        // Build uninstall directives from structured config.
        let uninstall_directives = build_uninstall_directives(cask_cfg.uninstall.as_ref());
        let zap_directives = build_uninstall_directives(cask_cfg.zap.as_ref());

        let empty_vec: Vec<String> = Vec::new();
        let binaries = cask_cfg.binaries.as_deref().unwrap_or_else(|| {
            // GoReleaser defaults binaries to [name]
            &empty_vec
        });
        let default_binaries;
        let binaries = if binaries.is_empty() {
            default_binaries = vec![cask_name.to_string()];
            &default_binaries
        } else {
            binaries
        };

        // Build depends_on directives from structured config
        let depends_directives = build_depends_directives(cask_cfg.dependencies.as_deref());
        let conflicts_directives = build_conflicts_directives(cask_cfg.conflicts.as_deref());

        // Extract hooks
        let preflight = cask_cfg
            .hooks
            .as_ref()
            .and_then(|h| h.pre.as_ref())
            .and_then(|p| p.install.as_deref());
        let postflight = cask_cfg
            .hooks
            .as_ref()
            .and_then(|h| h.post.as_ref())
            .and_then(|p| p.install.as_deref());
        let uninstall_preflight = cask_cfg
            .hooks
            .as_ref()
            .and_then(|h| h.pre.as_ref())
            .and_then(|p| p.uninstall.as_deref());
        let uninstall_postflight = cask_cfg
            .hooks
            .as_ref()
            .and_then(|h| h.post.as_ref())
            .and_then(|p| p.uninstall.as_deref());

        // Extract completions
        let completions_bash = cask_cfg
            .completions
            .as_ref()
            .and_then(|c| c.bash.as_deref());
        let completions_zsh = cask_cfg.completions.as_ref().and_then(|c| c.zsh.as_deref());
        let completions_fish = cask_cfg
            .completions
            .as_ref()
            .and_then(|c| c.fish.as_deref());

        let manpages = cask_cfg.manpages.as_deref().unwrap_or(&empty_vec);

        let params = CaskParams {
            name: cask_name,
            display_name: cask_name,
            alternative_names: cask_cfg.alternative_names.as_deref().unwrap_or(&empty_vec),
            version: &version,
            sha256: &sha256,
            url: &url,
            homepage: cask_cfg.homepage.as_deref(),
            description: cask_cfg.description.as_deref(),
            app: cask_cfg.app.as_deref(),
            binaries,
            caveats: cask_cfg.caveats.as_deref(),
            zap: &zap_directives,
            uninstall: &uninstall_directives,
            custom_block: cask_cfg.custom_block.as_deref(),
            service: cask_cfg.service.as_deref(),
            manpages,
            completions_bash,
            completions_zsh,
            completions_fish,
            depends_on: &depends_directives,
            conflicts_with: &conflicts_directives,
            preflight,
            postflight,
            uninstall_preflight,
            uninstall_postflight,
            platforms: Vec::new(), // Top-level cask uses single artifact
        };

        let content = generate_cask(&params)?;

        // Clone tap repo, write cask, commit, push.
        let tmp_dir = tempfile::tempdir().context("homebrew_casks: create temp dir")?;
        let repo_path = tmp_dir.path();

        let token = crate::util::resolve_repo_token(ctx, repo_cfg, Some("HOMEBREW_TAP_TOKEN"));
        crate::util::clone_repo(
            repo_cfg,
            &repo_owner,
            &repo_name,
            token.as_deref(),
            repo_path,
            "homebrew_casks",
            log,
        )?;

        let cask_dir = repo_path.join(&directory);
        std::fs::create_dir_all(&cask_dir)
            .with_context(|| format!("homebrew_casks: create {} dir", directory))?;

        let cask_path = cask_dir.join(format!("{}.rb", cask_name));
        std::fs::write(&cask_path, &content)
            .with_context(|| format!("homebrew_casks: write cask file {}", cask_path.display()))?;

        log.status(&format!("wrote Homebrew cask: {}", cask_path.display()));

        // Render commit message.
        let commit_msg = render_commit_msg(
            cask_cfg.commit_msg_template.as_deref(),
            cask_name,
            &version,
            "cask",
        );

        let cask_lossy = cask_path.to_string_lossy();
        let commit_opts = crate::util::resolve_commit_opts(cask_cfg.commit_author.as_ref());
        let branch = crate::util::resolve_branch(repo_cfg);
        crate::util::commit_and_push_with_opts(
            repo_path,
            &[&cask_lossy],
            &commit_msg,
            branch,
            "homebrew_casks",
            &commit_opts,
        )?;

        log.status(&format!(
            "Homebrew tap {}/{} updated with cask '{}' in {}",
            repo_owner, repo_name, cask_name, directory
        ));

        // Submit a PR if pull_request.enabled is set.
        let pr_branch = branch.unwrap_or("main");
        crate::util::maybe_submit_pr(
            repo_path,
            repo_cfg,
            &repo_owner,
            &repo_name,
            pr_branch,
            &format!("Update {} cask to {}", cask_name, version),
            &format!(
                "## Cask\n- **Name**: {}\n- **Version**: {}\n\nAutomatically submitted by anodizer.",
                cask_name, version
            ),
            "homebrew_casks",
            log,
        );
    }

    Ok(())
}
