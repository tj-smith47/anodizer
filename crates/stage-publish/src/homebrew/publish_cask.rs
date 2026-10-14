//! `publish_cask` — a crate's cask: where it is published
//! ([`CrateCask`]), its render, and the push for a cask that does not ride
//! the formula's tap commit (no formula, or its own tap repo).
use super::cask_scope::{CaskGenResult, cask_render_key, generate_cask_from_context};
use super::commit_msg::render_commit_msg;
use anodizer_core::config::{HomebrewCaskConfig, HomebrewConfig, PublishConfig, RepositoryConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

/// The formula block a cask falls back to when the crate has none: no
/// skip gate, no URL template, no description of its own.
static NO_FORMULA: std::sync::LazyLock<HomebrewConfig> =
    std::sync::LazyLock::new(HomebrewConfig::default);

/// A crate's Homebrew Cask, read through [`PublishConfig::homebrew_cask`]
/// so `publish.homebrew_cask:` and `publish.homebrew.cask:` take one path.
#[derive(Clone, Copy)]
pub(crate) struct CrateCask<'a> {
    /// The crate's `publish.homebrew:` formula block, when it has one.
    pub(crate) formula: Option<&'a HomebrewConfig>,
    /// The cask settings.
    pub(crate) cask: &'a HomebrewCaskConfig,
}

impl<'a> CrateCask<'a> {
    /// The crate's cask, or `None` when it configures none.
    pub(crate) fn of(publish: &'a PublishConfig) -> Option<Self> {
        Some(Self {
            formula: publish.homebrew.as_ref(),
            cask: publish.homebrew_cask()?,
        })
    }

    /// The crate's cask, looked up by crate name.
    pub(crate) fn for_crate(ctx: &'a Context, crate_name: &str) -> Option<Self> {
        Self::of(
            crate::util::find_crate_in_universe(ctx, crate_name)?
                .publish
                .as_ref()?,
        )
    }

    /// The formula block whose tap commit carries this cask, or `None` when
    /// the cask is pushed on its own: the crate has no formula, or the
    /// cask's `repository:` differs from the formula's in any field (see
    /// `RepositoryConfig`'s `PartialEq`), because one commit and one pull
    /// request cannot honor two branches, tokens, transports or
    /// `pull_request:` blocks.
    pub(crate) fn same_tap(&self) -> Option<&'a HomebrewConfig> {
        let formula = self.formula?;
        match self.cask.repository.as_ref() {
            None => Some(formula),
            Some(own) => (formula.repository.as_ref() == Some(own)).then_some(formula),
        }
    }

    /// The block the cask falls back to for unset gates, URL template,
    /// description, homepage and commit settings: the formula, else nothing.
    pub(crate) fn parent(&self) -> &'a HomebrewConfig {
        self.formula.unwrap_or(&NO_FORMULA)
    }

    /// The tap the cask is pushed to: its own `repository:`, else the
    /// formula's.
    pub(crate) fn repository(&self) -> Option<&'a RepositoryConfig> {
        self.cask
            .repository
            .as_ref()
            .or(self.parent().repository.as_ref())
    }
}

/// True when the crate cask's effective skip gates trip for `crate_name`:
/// the cask `skip_upload` (per-cask wins, else the formula's `skip_upload`) is
/// truthy, or the effective `if:` (cask-level wins, else the formula's)
/// evaluates falsy. The skip line prints on `log` only when `report_skip`
/// is set, which the publisher does and the render-only passes do not, so
/// it prints once per command. A `skip_upload` warning prints through the
/// logger [`Context::claim_render_log`] hands the value's owner, so it
/// prints once per command in every mode.
pub(super) fn cask_skip_gates_trip(
    ctx: &Context,
    hb_cfg: &anodizer_core::config::HomebrewConfig,
    cask_cfg: &anodizer_core::config::HomebrewCaskConfig,
    crate_name: &str,
    log: &StageLogger,
    report_skip: bool,
) -> Result<bool> {
    // A cask that inherits the formula's `skip_upload` reads the formula's
    // value, so it warns under the formula's key and the one bad value warns
    // once.
    let (effective_skip, owner_key) = match cask_cfg.skip_upload.as_ref() {
        Some(own) => (Some(own), super::cask_render_key(crate_name)),
        None => (
            hb_cfg.skip_upload.as_ref(),
            super::formula_render_key(crate_name),
        ),
    };
    let warn_log = ctx.claim_render_log(&format!("{owner_key} skip_upload"), log);
    if crate::util::should_skip_upload(effective_skip, ctx, &warn_log, None)? {
        if report_skip {
            log.status(&crate::util::skip_upload_line(&format!(
                "homebrew cask for '{crate_name}'"
            )));
        }
        return Ok(true);
    }

    // Cask-level `if:` wins; if unset, fall back to the parent formula's `if:`
    // so a per-crate gate on homebrew covers both surfaces in one declaration.
    // An `if: ""` on the cask imposes nothing, so it must not shadow the
    // formula's real gate — `active_if_gate` folds it to absent on both sides.
    use anodizer_core::config::active_if_gate;
    let effective_if = active_if_gate(cask_cfg.if_condition.as_deref())
        .or(active_if_gate(hb_cfg.if_condition.as_deref()));
    let proceed = anodizer_core::config::evaluate_if_condition(
        effective_if,
        &format!("homebrew cask publisher for crate '{}'", crate_name),
        |t| ctx.render_template(t),
    )?;
    if !proceed && report_skip {
        log.status(&format!(
            "skipped homebrew cask for '{}' — `if` condition evaluated falsy",
            crate_name
        ));
    }
    Ok(!proceed)
}

/// Render the Ruby cask a live publish would write for `crate_name`,
/// whether it rides the formula's tap commit or is pushed on its own,
/// honoring the cask's effective `skip_upload` and `if:` (the cask's own,
/// else the formula's).
///
/// Returns `Ok(None)` when no cask is configured or the publisher would skip
/// it. The offline schema validator renders through this function and the
/// push paths through the same [`generate_cask_from_context`], so the
/// validated document is byte-for-byte what a release pushes. The gates
/// print no skip line: that line belongs to the publisher.
pub(crate) fn render_homebrew_cask_for_crate(
    ctx: &Context,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<CaskGenResult>> {
    let Some(crate_cask) = CrateCask::for_crate(ctx, crate_name) else {
        return Ok(None);
    };
    let parent = crate_cask.parent();
    if cask_skip_gates_trip(ctx, parent, crate_cask.cask, crate_name, log, false)? {
        return Ok(None);
    }
    let cask_result = generate_cask_from_context(ctx, crate_name, parent, crate_cask.cask, log)?;
    Ok(Some(cask_result))
}

/// Push `crate_name`'s cask on its own: to the cask's `repository:`, else
/// the formula's. A cask that shares the formula's tap is pushed here when
/// the formula's gates leave it out. Returns `Ok(true)` when a commit reached
/// the tap.
pub fn publish_cask(ctx: &mut Context, crate_name: &str, log: &StageLogger) -> Result<bool> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "homebrew")?;
    let crate_cask = CrateCask::of(publish)
        .ok_or_else(|| anyhow::anyhow!("homebrew cask: no cask config for '{}'", crate_name))?;
    // Cloned so the push site can record into `ctx` while the config is in use.
    let hb_cfg = &crate_cask.parent().clone();
    let cask_cfg = &crate_cask.cask.clone();
    let repository = crate_cask.repository().cloned();
    let commit_author = cask_cfg
        .commit_author
        .as_ref()
        .or(hb_cfg.commit_author.as_ref());
    let commit_msg_template = cask_cfg
        .commit_msg_template
        .as_deref()
        .or(hb_cfg.commit_msg_template.as_deref());

    // Evaluate the cask's effective skip gates (per-cask wins, else the
    // formula's) before resolving the tap repo so a skipped cask with no
    // `repository:` block returns quietly with nothing to resolve.
    if cask_skip_gates_trip(ctx, hb_cfg, cask_cfg, crate_name, log, true)? {
        return Ok(false);
    }

    // Resolve repository owner/name from `repository:` (RepositoryConfig).
    let Some((repo_owner, repo_name)) = crate::publisher_helpers::absorb_entry_skip(
        ctx,
        log,
        "homebrew-cask",
        crate_name,
        crate::util::resolve_repo_owner_name(repository.as_ref()).ok_or_else(|| {
            anodizer_core::pipe_skip::entry_skip(
                crate::publisher_helpers::MISSING_REPOSITORY_REASON,
            )
        }),
    )?
    else {
        return Ok(false);
    };
    let Some(pr_token) = crate::publisher_helpers::absorb_entry_skip(
        ctx,
        log,
        "homebrew-cask",
        crate_name,
        crate::publisher_helpers::pr_token_before_push(
            ctx,
            repository.as_ref(),
            "HOMEBREW_TAP_TOKEN",
        ),
    )?
    else {
        return Ok(false);
    };

    let version = ctx.version();
    let cask_name = super::cask::cask_name_for(cask_cfg.name.as_deref().unwrap_or(crate_name));

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would update Homebrew cask {}/{} for '{}'",
            repo_owner, repo_name, cask_name
        ));
        return Ok(false);
    }

    let cask_result = generate_cask_from_context(
        ctx,
        crate_name,
        hb_cfg,
        cask_cfg,
        &ctx.claim_render_log(&cask_render_key(crate_name), log),
    )?;

    // Clone tap repo, write cask, commit, push.
    let tmp_dir = tempfile::tempdir().context("homebrew cask: create temp dir")?;
    let repo_path = tmp_dir.path();

    let token =
        crate::util::resolve_repo_token(ctx, repository.as_ref(), Some("HOMEBREW_TAP_TOKEN"))?;
    crate::util::clone_repo(
        ctx,
        repository.as_ref(),
        &repo_owner,
        &repo_name,
        token.as_deref(),
        repo_path,
        "homebrew cask",
        log,
    )?;

    // Honor `cask_cfg.directory:` so the tap can place casks in a sub-tree
    // (e.g. `Casks/versioned/`) instead of always ending up under `Casks/`.
    // Directory defaults to "Casks".
    let directory = super::resolve_cask_directory(cask_cfg.directory.as_deref(), ctx)?;
    let casks_dir = repo_path.join(&directory);
    std::fs::create_dir_all(&casks_dir).with_context(|| {
        format!(
            "homebrew cask: create {} dir {}",
            directory,
            casks_dir.display()
        )
    })?;

    let cask_path = casks_dir.join(format!("{}.rb", cask_result.cask_name));
    std::fs::write(&cask_path, &cask_result.content)
        .with_context(|| format!("homebrew cask: write cask file {}", cask_path.display()))?;
    log.status(&format!("wrote Homebrew cask {}", cask_path.display()));

    // `alternative_names:` versioned-file emission. Each entry that
    // renders to a token containing `@` (e.g. `myapp@1.2.3`) becomes its
    // own `.rb` file so `brew install myapp@1.2.3` installs a pinned
    // version. Aliases without `@` are rendered inline as `name "..."`
    // directives (handled in `generate_cask_from_context`).
    let mut written_paths: Vec<std::path::PathBuf> = vec![cask_path.clone()];
    for (alt_name, body) in &cask_result.versioned_files {
        let alt_path = casks_dir.join(format!("{}.rb", alt_name));
        std::fs::write(&alt_path, body).with_context(|| {
            format!(
                "homebrew cask: write versioned cask file {}",
                alt_path.display()
            )
        })?;
        log.status(&format!("wrote Homebrew cask {}", alt_path.display()));
        written_paths.push(alt_path);
    }

    let commit_msg = render_commit_msg(
        commit_msg_template,
        &cask_result.cask_name,
        &version,
        "cask",
        log,
        ctx.render_is_strict(),
    )?;

    let path_strings: Vec<String> = written_paths
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let path_refs: Vec<&str> = path_strings.iter().map(String::as_str).collect();
    let commit_opts = crate::util::resolve_commit_opts(ctx, commit_author, log)?;
    let branch = crate::util::resolve_branch_or_versioned(
        ctx,
        repository.as_ref(),
        &cask_result.cask_name,
        &version,
    );
    // Rendered before the push: a template error here must not come
    // after a commit already reached the tap.
    let update_existing_pr = match cask_cfg.update_existing_pr.as_ref() {
        Some(v) => v
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .context("homebrew cask: render update_existing_pr condition")?,
        None => false,
    };
    let outcome = crate::util::commit_and_push_with_opts(
        repo_path,
        &path_refs,
        &commit_msg,
        branch.as_deref(),
        "homebrew cask",
        &commit_opts,
        log,
    )?;
    match &outcome {
        crate::util::CommitOutcome::Pushed { commit } => {
            super::publisher::record_tap_push(
                ctx,
                crate_name.to_string(),
                &repo_owner,
                &repo_name,
                branch.as_deref(),
                commit,
            );
            log.status(&format!(
                "Homebrew tap {}/{} updated with cask '{}'",
                repo_owner, repo_name, cask_result.cask_name
            ));
        }
        crate::util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "nothing to push for homebrew cask — '{}' already up to date",
                cask_result.cask_name
            ));
        }
    }

    // Submit a PR if pull_request.enabled is set.
    let pr_branch = branch.as_deref().unwrap_or("main");
    let pr_outcome = crate::util::maybe_submit_pr(
        repo_path,
        repository.as_ref(),
        &crate::util::PrOrigin {
            repo_owner: &repo_owner,
            repo_name: &repo_name,
            branch_name: pr_branch,
            update_existing_pr,
        },
        &super::publish_top::cask_pr_title(&cask_result.cask_name, &version),
        &format!(
            "## Cask\n- **Name**: {}\n- **Version**: {}\n\n{}",
            cask_result.cask_name,
            version,
            crate::util::SUBMITTED_BY_FOOTER
        ),
        "homebrew cask",
        log,
        pr_token.as_deref(),
        &|s| ctx.render_template(s),
    )?;

    super::publisher::record_tap_pr_url(ctx, outcome.commit(), pr_outcome.url);
    // Surface PR-already-exists skips to the dispatch summary table.
    if let Some(outcome) = pr_outcome.outcome {
        ctx.record_publisher_outcome(outcome);
    }

    Ok(matches!(outcome, crate::util::CommitOutcome::Pushed { .. }))
}
