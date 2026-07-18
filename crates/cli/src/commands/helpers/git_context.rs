use anodizer_core::config::Config;
use anodizer_core::context::Context;
use anodizer_core::git;
use anodizer_core::log::StageLogger;

/// Resolve the current-tag override from the env-var precedence chain.
///
/// Precedence (first non-empty wins):
///   1. `ANODIZER_CURRENT_TAG`
///   2. `GORELEASER_CURRENT_TAG` (compat alias)
///   3. `GITHUB_REF_NAME`, but only when `GITHUB_REF_TYPE == "tag"` — GitHub
///      Actions exposes the triggering tag here on a tag push, while a branch
///      push puts the branch name in the same var (which is not a tag).
pub(super) fn resolve_tag_override(
    anodizer_current_tag: Option<String>,
    goreleaser_current_tag: Option<String>,
    github_ref_type: Option<String>,
    github_ref_name: Option<String>,
) -> Option<String> {
    anodizer_current_tag
        .filter(|s| !s.is_empty())
        .or_else(|| goreleaser_current_tag.filter(|s| !s.is_empty()))
        .or_else(|| {
            let is_tag = github_ref_type.as_deref().filter(|s| *s == "tag").is_some();
            if is_tag {
                github_ref_name.filter(|s| !s.is_empty())
            } else {
                None
            }
        })
}

/// Resolve tag and populate git variables on the context.
///
/// Finds the first selected crate (or the first crate in config), looks up
/// the latest tag matching its `tag_template`, detects git info, and
/// populates the context's template variables.
pub fn resolve_git_context(
    ctx: &mut Context,
    config: &Config,
    log: &StageLogger,
) -> anyhow::Result<()> {
    // Warn on shallow clones where tag discovery may be incomplete.
    if git::is_shallow_clone() {
        log.warn(
            "shallow clone detected; tag discovery may be incomplete. \
             Use `git fetch --unshallow` in CI.",
        );
    }

    // Allow env var overrides for tag discovery. Anodizer-native var wins;
    // a compat alias is checked as a fallback so CI jobs migrating
    // pick up their existing env vars without rewiring. As a
    // last resort, GitHub Actions exposes the triggering tag as GITHUB_REF_NAME
    // when GITHUB_REF_TYPE=tag — use that so workflows that didn't explicitly
    // export ANODIZER_CURRENT_TAG (e.g. `Release.yml` jobs dispatched by a tag
    // push) still resolve the correct tag instead of falling through to
    // per-crate-template latest-tag scanning (which can mis-resolve when the
    // triggering tag's prefix doesn't match the first crate's tag_template).
    let anodizer_current_tag = ctx.env_var("ANODIZER_CURRENT_TAG");
    let goreleaser_current_tag = ctx.env_var("GORELEASER_CURRENT_TAG");
    let github_ref_type = ctx.env_var("GITHUB_REF_TYPE");
    let github_ref_name = ctx.env_var("GITHUB_REF_NAME");
    tracing::debug!(
        anodizer_current_tag = ?anodizer_current_tag,
        goreleaser_current_tag = ?goreleaser_current_tag,
        github_ref_type = ?github_ref_type,
        github_ref_name = ?github_ref_name,
        "tag_override resolution: env var snapshot"
    );
    let tag_override = resolve_tag_override(
        anodizer_current_tag,
        goreleaser_current_tag,
        github_ref_type,
        github_ref_name,
    );

    // Resolve a crate to derive the tag from. Selection order:
    //   1. The first explicitly selected crate (--crate or --all selection)
    //   2. The first crate of the universe (top-level first, then workspace
    //      crates — the workspace fallback is critical for snapshot/dry-run
    //      mode in workspace-only configs like cfgd; without it, `Version`
    //      is never populated in the template context, breaking any
    //      template that references it).
    let first_crate = ctx
        .options
        .selected_crates
        .first()
        .and_then(|name| config.find_crate(name))
        .or_else(|| config.crate_universe().into_iter().next());

    if let Some(crate_cfg) = first_crate {
        // Resolve the crate's own full tag template once — the crate's raw
        // value if set, else the `{name}-v` convention (NOT
        // `resolved_tag_template()`'s built-in `v{{ Version }}` default,
        // which is the wrong family for per-crate `{name}-v` configs). Both
        // the latest-tag matcher below and the previous-tag prefix filter
        // extract from this SAME resolved template so they never drift
        // into mismatched families for an unset-template crate.
        let crate_tag_template = crate_cfg
            .tag_template
            .clone()
            .unwrap_or_else(|| format!("{}-v{{{{ Version }}}}", crate_cfg.name));
        let tag = if let Some(ref override_tag) = tag_override {
            log.verbose(&format!(
                "using ANODIZER_CURRENT_TAG override '{}'",
                override_tag
            ));
            override_tag.clone()
        } else {
            let monorepo_prefix = config.monorepo_tag_prefix();
            let latest_tag = match git::find_latest_tag_matching_with_prefix(
                &crate_tag_template,
                config.git.as_ref(),
                Some(ctx.template_vars()),
                monorepo_prefix,
            ) {
                Ok(found) => found,
                Err(e) => {
                    log.warn(&format!("error finding tags matching template: {e}"));
                    None
                }
            };
            match latest_tag {
                Some(t) => t,
                None => {
                    if ctx.options.snapshot || ctx.options.nightly {
                        let mode = if ctx.options.nightly {
                            "nightly"
                        } else {
                            "snapshot"
                        };
                        log.warn(&format!(
                            "no git tags found, defaulting to v0.0.0 ({mode} mode)."
                        ));
                        "v0.0.0".to_string()
                    } else if ctx.options.dry_run {
                        log.warn("no git tags found, defaulting to v0.0.0 (dry-run mode).");
                        "v0.0.0".to_string()
                    } else if ctx.options.preflight_secrets {
                        // The pre-tag secrets gate runs before a tag exists at
                        // HEAD; it validates only secret presence, so a synthetic
                        // v0.0.0 suffices to render any `{{ .Env.* }}` refs.
                        "v0.0.0".to_string()
                    } else if ctx.options.notify {
                        // A notification must not be blocked by the absence of a
                        // tag; the synthetic v0.0.0 lets any `{{ Tag }}` ref render
                        // (raw on_error messages skip rendering entirely).
                        "v0.0.0".to_string()
                    } else {
                        anyhow::bail!("no git tag found; create a tag or use --snapshot");
                    }
                }
            }
        };

        // Validate HEAD points at the tag.
        // Skip this check for the synthetic v0.0.0 tag since it doesn't exist in git.
        // The standalone `changelog` preview also skips it: an inspection tool
        // must render a tag's window without requiring the operator to check
        // that tag out (the release pipeline never sets `changelog_preview`).
        let is_synthetic_tag = tag == "v0.0.0" && tag_override.is_none();
        if !is_synthetic_tag
            && let Ok(false) = git::tag_points_at_head(&tag)
            && !ctx.options.snapshot
            && !ctx.options.nightly
            && !ctx.options.changelog_preview
            && !ctx.options.preflight_secrets
            && !ctx.options.notify
        {
            let head = git::get_short_commit().unwrap_or_else(|_| "unknown".to_string());
            anyhow::bail!(
                "tag {} does not point at HEAD ({}). Check out the tag or use --snapshot to skip this check.",
                tag,
                head
            );
        }

        match git::detect_git_info(&tag, ctx.skip_validate()) {
            Ok(mut git_info) => {
                // Validate dirty working tree: error in non-snapshot/non-dry-run mode,
                // a dirty-tree check. The standalone `changelog` preview skips
                // it too — a local inspection must not require a clean tree.
                if git_info.dirty
                    && !ctx.options.snapshot
                    && !ctx.options.nightly
                    && !ctx.options.changelog_preview
                    && !ctx.options.preflight_secrets
                    && !ctx.options.notify
                {
                    if ctx.options.dry_run {
                        log.warn("git is in a dirty state; run `git status` to see what changed.");
                    } else {
                        anyhow::bail!(
                            "git is in a dirty state; run `git status` to see what changed. \
                             Use --snapshot to force."
                        );
                    }
                }

                // Allow ANODIZER_PREVIOUS_TAG (or the compat
                // GORELEASER_PREVIOUS_TAG) env override for the previous tag.
                let prev_override = ctx
                    .env_var("ANODIZER_PREVIOUS_TAG")
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        ctx.env_var("GORELEASER_PREVIOUS_TAG")
                            .filter(|s| !s.is_empty())
                    });
                if let Some(prev_override) = prev_override {
                    log.verbose(&format!(
                        "using ANODIZER_PREVIOUS_TAG override '{}'",
                        prev_override
                    ));
                    git_info.previous_tag = Some(prev_override);
                } else {
                    // Derive the tag-prefix filter from the current crate's
                    // tag_template (e.g. `v` for cfgd, `csi-v` for cfgd-csi)
                    // so monorepo-style workspaces don't bleed prior tags
                    // across crates. Without this, `git describe --tags`
                    // returns the most recent tag of ANY crate — e.g.
                    // `cfgd: csi-v0.3.4 -> 0.3.5` ends up in the nix/
                    // homebrew commit message because csi was the most
                    // recently tagged sibling. Falls back to the global
                    // monorepo prefix when the template has no extractable
                    // prefix.
                    let crate_prefix = git::extract_tag_prefix(&crate_tag_template);
                    let prefix = crate_prefix
                        .as_deref()
                        .or_else(|| config.monorepo_tag_prefix());
                    git_info.previous_tag = git::find_previous_tag_with_prefix(
                        &tag,
                        config.git.as_ref(),
                        Some(ctx.template_vars()),
                        prefix,
                    )
                    .ok()
                    .flatten();
                }
                ctx.git_info = Some(git_info);
                ctx.populate_git_vars();
            }
            Err(e) => {
                // snapshot/nightly tolerate a tagless or HEADless repo (defaults
                // stand in); notify joins them — a notification side-channel must
                // not fail because git info can't be detected (e.g. a release that
                // never reached a commit, or an on_error hook in a fresh repo).
                let lenient_mode = if ctx.options.nightly {
                    Some("nightly")
                } else if ctx.options.snapshot {
                    Some("snapshot")
                } else if ctx.options.notify {
                    Some("notify")
                } else {
                    None
                };
                if let Some(mode) = lenient_mode {
                    log.warn(&format!(
                        "could not detect git info in {mode} mode, using defaults: {e}"
                    ));
                    ctx.git_info = Some(git::GitInfo {
                        tag: tag.clone(),
                        commit: "none".to_string(),
                        short_commit: "none".to_string(),
                        branch: "none".to_string(),
                        dirty: true,
                        semver: git::SemVer {
                            major: 0,
                            minor: 0,
                            patch: 0,
                            prerelease: None,
                            build_metadata: None,
                        },
                        commit_date: String::new(),
                        commit_timestamp: String::new(),
                        previous_tag: None,
                        remote_url: String::new(),
                        summary: mode.to_string(),
                        tag_subject: String::new(),
                        tag_contents: String::new(),
                        tag_body: String::new(),
                        first_commit: None,
                    });
                    ctx.populate_git_vars();
                } else {
                    return Err(anyhow::anyhow!("could not detect git info: {e}"));
                }
            }
        }
    } else {
        ctx.populate_git_vars();
    }
    Ok(())
}
