//! `publish_top_level_homebrew_casks` — emits cask `.rb` files from the
//! top-level `homebrew_casks:` config block (independent of any per-crate
//! homebrew config).
use super::cask::{
    CaskParams, find_top_level_cask_artifact, generate_cask, render_additional_url_params,
    render_uninstall_block, render_zap_block,
};
use super::commit_msg::render_commit_msg;
use super::formula::{build_conflicts_directives, build_depends_directives};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};
/// Render and push every entry in `homebrew_casks:`. Returns `Ok(true)`
/// when at least one cask was actually pushed to its tap repo; `Ok(false)`
/// when every entry skipped (no config, skip_upload, dry-run). The
/// boolean feeds back into [`super::publisher::HomebrewPublisher::run`]
/// so the rollback orchestrator doesn't trip git-revert on a tap that
/// this run never touched.
pub fn publish_top_level_homebrew_casks(ctx: &mut Context, log: &StageLogger) -> Result<bool> {
    // Clone the entries so the loop body can call `&mut Context`
    // helpers (e.g. `ctx.record_publisher_outcome`) without holding
    // an immutable borrow on `ctx.config.homebrew_casks` across the
    // mutation. The top-level cask list is bounded (a handful of
    // entries per release) so the clone cost is negligible.
    let entries = match ctx.config.homebrew_casks {
        Some(ref v) if !v.is_empty() => v.clone(),
        _ => return Ok(false),
    };
    let mut pushed_any = false;

    for cask_cfg in &entries {
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
                crate::util::render_url_template_with_ctx(
                    ctx,
                    tmpl,
                    macos_artifact.name(),
                    &version,
                    &arch,
                    &os,
                )
            } else {
                macos_artifact.metadata.get("url").cloned().ok_or_else(|| {
                    anyhow::anyhow!(
                        "homebrew_casks: artifact for cask '{}' has no 'url' metadata \
                             and no url.template configured to synthesize one. A cask with \
                             an empty `url \"\"` line is rejected by `brew style` and fails \
                             on `brew install` (no download endpoint). Either set \
                             `homebrew_casks[].url.template` to render a URL from \
                             `{{{{ .Tag }}}}` / `{{{{ .Os }}}}` / `{{{{ .Arch }}}}`, or \
                             ensure the release stage seeds `metadata.url` onto the \
                             macOS artifact for '{}'.",
                        cask_name,
                        cask_name
                    )
                })?
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

        // Pre-render multi-key uninstall + zap blocks (GR parity, see
        // `cask::render_zap_block` doc-comment).
        let uninstall_block = render_uninstall_block(cask_cfg.uninstall.as_ref());
        let zap_block = render_zap_block(cask_cfg.zap.as_ref());

        // Pre-render Ruby kwargs continuation for the `url` line —
        // mirrors GR `internal/pipe/cask/templates/additional_url_params.rb`.
        let url_extras_top = cask_cfg
            .url
            .as_ref()
            .map(|u| render_additional_url_params(u, "      "))
            .unwrap_or_default();
        let url_extras_arch = cask_cfg
            .url
            .as_ref()
            .map(|u| render_additional_url_params(u, "        "))
            .unwrap_or_default();

        let empty_vec: Vec<String> = Vec::new();
        // Map config-side `HomebrewCaskBinary` (untagged enum: bare string OR
        // `{ name, target }`) into the template-side `CaskBinaryEntry` shape
        // — same translation used in the per-crate cask renderer. Defaults
        // to `[{ name: cask_name, target: None }]` so the bare default still
        // emits `binary "<n>"`.
        let configured_binaries: Vec<super::cask::CaskBinaryEntry> = cask_cfg
            .binaries
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|b| super::cask::CaskBinaryEntry {
                name: b.name().to_string(),
                target: b.target().map(str::to_string),
            })
            .collect();
        let default_binaries;
        let binaries: &[super::cask::CaskBinaryEntry] = if configured_binaries.is_empty() {
            default_binaries = vec![super::cask::CaskBinaryEntry {
                name: cask_name.to_string(),
                target: None,
            }];
            &default_binaries
        } else {
            &configured_binaries
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
        let completions_bash = cask_cfg.completion.as_ref().and_then(|c| c.bash.as_deref());
        let completions_zsh = cask_cfg.completion.as_ref().and_then(|c| c.zsh.as_deref());
        let completions_fish = cask_cfg.completion.as_ref().and_then(|c| c.fish.as_deref());

        let manpages = cask_cfg.manpages.as_deref().unwrap_or(&empty_vec);

        let params = CaskParams {
            name: cask_name,
            display_name: cask_name,
            alternative_names: cask_cfg.alternative_names.as_deref().unwrap_or(&empty_vec),
            version: &version,
            sha256: &sha256,
            url: &url,
            url_extras: &url_extras_top,
            url_extras_indented: &url_extras_arch,
            homepage: cask_cfg.homepage.as_deref(),
            description: cask_cfg.description.as_deref(),
            app: cask_cfg.app.as_deref(),
            binaries,
            caveats: cask_cfg.caveats.as_deref(),
            zap_block: &zap_block,
            uninstall_block: &uninstall_block,
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
            generate_completions: cask_cfg
                .generate_completions_from_executable
                .as_ref()
                .and_then(super::cask::render_generate_completions),
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
        let commit_opts = crate::util::resolve_commit_opts(ctx, cask_cfg.commit_author.as_ref());
        let branch = crate::util::resolve_branch(repo_cfg);
        let outcome = crate::util::commit_and_push_with_opts(
            repo_path,
            &[&cask_lossy],
            &commit_msg,
            branch,
            "homebrew_casks",
            &commit_opts,
        )?;
        match outcome {
            crate::util::CommitOutcome::Pushed => {
                pushed_any = true;
                log.status(&format!(
                    "Homebrew tap {}/{} updated with cask '{}' in {}",
                    repo_owner, repo_name, cask_name, directory
                ));
            }
            crate::util::CommitOutcome::NoChanges => {
                log.status(&format!(
                    "homebrew_casks: nothing to push, cask '{}' already up to date",
                    cask_name
                ));
            }
        }

        // Submit a PR if pull_request.enabled is set.
        let pr_branch = branch.unwrap_or("main");
        let update_existing_pr = cask_cfg
            .update_existing_pr
            .as_ref()
            .map(|v| {
                v.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        let pr_outcome = crate::util::maybe_submit_pr(
            repo_path,
            repo_cfg,
            &crate::util::PrOrigin {
                repo_owner: &repo_owner,
                repo_name: &repo_name,
                branch_name: pr_branch,
                update_existing_pr,
            },
            &format!("Update {} cask to {}", cask_name, version),
            &format!(
                "## Cask\n- **Name**: {}\n- **Version**: {}\n\nAutomatically submitted by anodizer.",
                cask_name, version
            ),
            "homebrew_casks",
            log,
        );

        // Sticky-pending: once any cask in this top-level group
        // records a Pending outcome (e.g. PR-already-exists skip), a
        // subsequent successful cask must NOT clear it. The dispatch
        // row reports the most cautious status across the entire
        // group — "succeeded" would be a lie if even one cask
        // skipped. Implementation: only call `record_publisher_outcome`
        // on the `Some(outcome)` arm; the `None` (success) arm leaves
        // the slot untouched. Iteration order across casks is
        // therefore irrelevant.
        if let Some(outcome) = pr_outcome {
            ctx.record_publisher_outcome(outcome);
        }
    }

    Ok(pushed_any)
}

#[cfg(test)]
mod tests {
    use anodizer_core::PublisherOutcome;
    use anodizer_core::context::Context;

    /// Sticky-pending semantic: a cask that records `PendingValidation`
    /// followed by a cask that records nothing must leave the slot at
    /// `PendingValidation`. Models "cask A's PR already exists; cask B
    /// pushed cleanly" — the group row must still read pending.
    #[test]
    fn sticky_pending_preserves_pending_when_next_cask_succeeds() {
        let mut ctx = Context::test_fixture();
        // Cask A: PR already exists → records PendingValidation.
        if let Some(outcome) = Some(PublisherOutcome::PendingValidation) {
            ctx.record_publisher_outcome(outcome);
        }
        // Cask B: succeeded → returns None; loop does not call
        // `record_publisher_outcome`, so the slot stays at Pending.
        let pr_outcome_b: Option<PublisherOutcome> = None;
        if let Some(outcome) = pr_outcome_b {
            ctx.record_publisher_outcome(outcome);
        }
        assert!(matches!(
            ctx.take_pending_outcome(),
            Some(PublisherOutcome::PendingValidation)
        ));
    }

    /// Converse: a cask that records nothing followed by a cask that
    /// records `PendingValidation` must leave the slot at
    /// `PendingValidation`. Order across casks is irrelevant —
    /// any single pending cask wins.
    #[test]
    fn sticky_pending_records_pending_when_later_cask_skips() {
        let mut ctx = Context::test_fixture();
        let pr_outcome_a: Option<PublisherOutcome> = None;
        if let Some(outcome) = pr_outcome_a {
            ctx.record_publisher_outcome(outcome);
        }
        if let Some(outcome) = Some(PublisherOutcome::PendingValidation) {
            ctx.record_publisher_outcome(outcome);
        }
        assert!(matches!(
            ctx.take_pending_outcome(),
            Some(PublisherOutcome::PendingValidation)
        ));
    }

    /// Baseline: when every cask succeeds (no Pending arm fires) the
    /// slot remains empty and dispatch defaults to Succeeded. Guards
    /// against accidentally clearing-then-recording None.
    #[test]
    fn sticky_pending_leaves_slot_empty_when_all_casks_succeed() {
        let mut ctx = Context::test_fixture();
        let outcomes: [Option<PublisherOutcome>; 2] = [None, None];
        for outcome in outcomes.into_iter().flatten() {
            ctx.record_publisher_outcome(outcome);
        }
        assert!(ctx.take_pending_outcome().is_none());
    }
}
