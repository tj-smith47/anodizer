//! `Stage` impl for `ChangelogStage` — the pipeline entry point.
//!
//! Orchestrates per-crate changelog generation: resolves config, fetches
//! commits via the configured backend (`use: git` / `github` / `gitlab` /
//! `gitea` / `github-native`), filters / sorts / groups, renders Markdown,
//! and writes `dist/CHANGELOG.md`.

use std::path::PathBuf;

use anodizer_core::config::{ChangelogConfig, ChangelogGroup, ContentSource};
use anodizer_core::context::Context;
use anodizer_core::git::find_latest_tag_matching_with_prefix;
use anodizer_core::log::StageLogger;
use anodizer_core::stage::Stage;
use anyhow::{Context as _, Result, bail};

use crate::fetch::{
    fetch_git_commits, fetch_gitea_commits, fetch_github_commits, fetch_gitlab_commits,
    should_preempt_scm_to_git,
};
use crate::group::{
    CommitInfo, GroupedCommits, apply_filters, apply_include_filters, group_commits, sort_commits,
};
use crate::render::{ChangelogRenderOpts, render_changelog_with_provider};

/// Resolved options for the native changelog pipeline (everything not
/// touched by the `release-notes` / `github-native` early returns).
struct ChangelogOpts {
    sort_order: String,
    exclude_filters: Vec<String>,
    include_filters: Vec<String>,
    groups: Vec<ChangelogGroup>,
    header: Option<String>,
    footer: Option<String>,
    abbrev: i32,
    format_template: Option<String>,
    paths: Vec<String>,
    title: Option<String>,
    divider: Option<String>,
}

impl Stage for super::ChangelogStage {
    fn name(&self) -> &str {
        "changelog"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("changelog");

        let changelog_cfg = ctx.config.changelog.clone();

        // Snapshot-mode opt-in (matches GoReleaser's `if ctx.Snapshot { skip }`
        // default; user opts back in via `changelog.snapshot: true` for local
        // preview / draft generation).
        if ctx.is_snapshot() {
            let snapshot_opt_in = changelog_cfg
                .as_ref()
                .map(|c| c.resolved_snapshot())
                .unwrap_or(false);
            if !snapshot_opt_in {
                log.status(
                    "changelog skipped (snapshot mode; set `changelog.snapshot: true` to render)",
                );
                return Ok(());
            }
        }

        if handle_release_notes_override(ctx, &log)? {
            return Ok(());
        }

        // If skipped, skip the stage entirely (supports template-conditional skip).
        if let Some(d) = changelog_cfg.as_ref().and_then(|c| c.skip.as_ref()) {
            let off = d
                .try_evaluates_to_true(|s| ctx.render_template(s))
                .with_context(|| "changelog: render skip template")?;
            if off {
                log.status("changelog skipped");
                return Ok(());
            }
        }

        // If `use: github-native`, skip changelog generation and store empty
        // bodies so the release stage can delegate to GitHub's auto-generated
        // release notes.
        let use_source = changelog_cfg
            .as_ref()
            .map(|c| c.resolved_use_source().to_string())
            .unwrap_or_else(|| {
                anodizer_core::config::ChangelogConfig::DEFAULT_USE_SOURCE.to_string()
            });

        if use_source == "github-native" {
            handle_github_native_changelog(ctx, &log, changelog_cfg.as_ref())?;
            return Ok(());
        }

        // Validate the use source against ChangelogConfig::VALID_USE_SOURCE
        // (excluding github-native which the early-return handled above).
        if !anodizer_core::config::ChangelogConfig::VALID_USE_SOURCE.contains(&use_source.as_str())
        {
            anyhow::bail!(
                "changelog: unsupported use source {:?} (expected one of: {})",
                use_source,
                anodizer_core::config::ChangelogConfig::VALID_USE_SOURCE.join(", ")
            );
        }

        let opts = resolve_changelog_opts(ctx, &log, changelog_cfg.as_ref())?;

        let selected = ctx.options.selected_crates.clone();
        let dist = ctx.config.dist.clone();

        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        let mut combined_markdown = String::new();
        for crate_cfg in &crates {
            let markdown = render_crate_changelog(ctx, &log, crate_cfg, &opts, &use_source)?;
            ctx.stage_outputs
                .changelogs
                .insert(crate_cfg.name.clone(), markdown.clone());
            combined_markdown.push_str(&markdown);
        }

        let final_markdown = wrap_with_header_footer(
            ctx,
            &log,
            &combined_markdown,
            opts.header.as_deref(),
            opts.footer.as_deref(),
        )?;

        write_changelog_dist(&log, &dist, &final_markdown)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Honour `--release-notes <path>`: read the file, fan it out to every
/// selected crate, and write `dist/CHANGELOG.md`. Returns `true` when the
/// override path was taken (caller short-circuits the rest of the stage).
fn handle_release_notes_override(ctx: &mut Context, log: &StageLogger) -> Result<bool> {
    let Some(notes_path) = ctx.options.release_notes_path.clone() else {
        return Ok(false);
    };
    let content = std::fs::read_to_string(&notes_path).with_context(|| {
        format!(
            "changelog: failed to read release notes file: {}",
            notes_path.display()
        )
    })?;
    if content.trim().is_empty() {
        log.warn(&format!(
            "release notes file {} is empty; release body will be blank",
            notes_path.display()
        ));
    }
    log.status(&format!(
        "using custom release notes from {}",
        notes_path.display()
    ));

    let selected = ctx.options.selected_crates.clone();
    let crates: Vec<_> = ctx
        .config
        .crates
        .iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .cloned()
        .collect();
    for crate_cfg in &crates {
        ctx.stage_outputs
            .changelogs
            .insert(crate_cfg.name.clone(), content.clone());
    }

    let dist = ctx.config.dist.clone();
    std::fs::create_dir_all(&dist)
        .with_context(|| format!("changelog: create dist dir {}", dist.display()))?;
    let notes_out = dist.join("CHANGELOG.md");
    std::fs::write(&notes_out, &content)
        .with_context(|| format!("changelog: write {}", notes_out.display()))?;
    log.status(&format!("wrote {}", notes_out.display()));
    Ok(true)
}

/// Handle `use: github-native`: call GitHub's generate-notes endpoint per
/// crate, wrap with header/footer, and write `dist/CHANGELOG.md`. Mirrors
/// GoReleaser's `githubNativeChangeloger.Log`.
fn handle_github_native_changelog(
    ctx: &mut Context,
    log: &StageLogger,
    changelog_cfg: Option<&ChangelogConfig>,
) -> Result<()> {
    if ctx.options.token.is_none() && !ctx.is_dry_run() && !ctx.is_snapshot() {
        bail!(
            "changelog: use=github-native requires a GitHub token (set \
             GITHUB_TOKEN or ANODIZER_GITHUB_TOKEN, or pass --token); \
             GitHub auto-release-notes is an authenticated API"
        );
    }
    let has_repo = ctx.config.crates.iter().any(|c| {
        c.release
            .as_ref()
            .and_then(|r| r.github.as_ref())
            .is_some_and(|g| !g.owner.is_empty() && !g.name.is_empty())
    });
    if !has_repo && !ctx.is_dry_run() && !ctx.is_snapshot() {
        bail!(
            "changelog: use=github-native requires release.github.owner and \
             release.github.name on at least one crate so the auto-release-notes \
             API knows which repository to read"
        );
    }

    let monorepo_prefix = ctx.config.monorepo_tag_prefix();
    let current_tag = ctx.template_vars().get("Tag").cloned().unwrap_or_default();
    let token = ctx.options.token.clone();
    let dry_run_or_snapshot = ctx.is_dry_run() || ctx.is_snapshot();

    let selected = ctx.options.selected_crates.clone();
    let crates: Vec<_> = ctx
        .config
        .crates
        .iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .cloned()
        .collect();

    let mut combined = String::new();
    // Tracks whether at least one per-crate body came from the real
    // GitHub generate-notes API (vs. all empty placeholders because
    // every crate lacked `release.github`). Used to set
    // `github_native_changelog` accurately — the flag signals "body
    // provenance is GitHub" to downstream consumers, so it must not be
    // set when every body is empty.
    let mut any_github_body = false;
    for crate_cfg in &crates {
        let github_cfg = crate_cfg.release.as_ref().and_then(|r| r.github.as_ref());
        let Some(repo) = github_cfg else {
            // Warn (not status) so a missing `release.github` on
            // a crate that should have one is visible in CI output
            // instead of buried in info-level logs.
            log.warn(&format!(
                "changelog: use=github-native but crate '{}' has no release.github \
                 config — skipping (no GitHub release body will be generated for \
                 this crate; if it should have a release, add release.github.owner \
                 and release.github.name)",
                crate_cfg.name
            ));
            ctx.stage_outputs
                .changelogs
                .insert(crate_cfg.name.clone(), String::new());
            continue;
        };

        let prev_tag = find_latest_tag_matching_with_prefix(
            &crate_cfg.tag_template,
            ctx.config.git.as_ref(),
            Some(ctx.template_vars()),
            monorepo_prefix,
        )
        .unwrap_or(None)
        .filter(|t| t.as_str() != current_tag.as_str());

        let body = if dry_run_or_snapshot {
            log.status(&format!(
                "(dry-run/snapshot) would call POST /repos/{}/{}/releases/generate-notes \
                 (tag_name={:?}, previous_tag_name={:?})",
                repo.owner, repo.name, current_tag, prev_tag
            ));
            String::new()
        } else {
            log.status(&format!(
                "github-native: fetching release notes for {}/{} \
                 (tag_name={:?}, previous_tag_name={:?})",
                repo.owner, repo.name, current_tag, prev_tag
            ));
            crate::github_native::generate_release_notes(
                &repo.owner,
                &repo.name,
                &current_tag,
                prev_tag.as_deref(),
                token.as_deref(),
            )
            .with_context(|| {
                format!(
                    "changelog: github-native generate-notes for crate '{}'",
                    crate_cfg.name
                )
            })?
        };

        ctx.stage_outputs
            .changelogs
            .insert(crate_cfg.name.clone(), body.clone());
        combined.push_str(&body);
        // generate-notes returns 2xx with empty content when no
        // PRs/commits sit between the two tags; only flip the
        // provenance flag in real runs when the body is non-empty so
        // downstream consumers aren't misled. In dry-run/snapshot the
        // body is always empty but the intent is still GitHub-sourced.
        if dry_run_or_snapshot || !body.trim().is_empty() {
            any_github_body = true;
        }
    }

    ctx.stage_outputs.github_native_changelog = any_github_body;

    let header_src: Option<ContentSource> = changelog_cfg.and_then(|c| c.header.clone());
    let footer_src: Option<ContentSource> = changelog_cfg.and_then(|c| c.footer.clone());
    let header: Option<String> = header_src
        .as_ref()
        .map(|src| anodizer_core::content_source::resolve(src, "changelog header", ctx))
        .transpose()?;
    let footer: Option<String> = footer_src
        .as_ref()
        .map(|src| anodizer_core::content_source::resolve(src, "changelog footer", ctx))
        .transpose()?;

    // Header/footer in this path use unconditional emission semantics
    // (empty rendered content is still included with surrounding
    // whitespace), so it does not go through `wrap_with_header_footer`.
    let mut final_markdown = String::new();
    let mut rendered_header: Option<String> = None;
    if let Some(ref h) = header {
        let rendered = ctx.render_template_strict(h, "changelog header", log)?;
        if !rendered.trim().is_empty() {
            final_markdown.push_str(&rendered);
            final_markdown.push_str("\n\n");
            rendered_header = Some(rendered);
        }
    }
    final_markdown.push_str(&combined);
    let mut rendered_footer: Option<String> = None;
    if let Some(ref f) = footer {
        let rendered = ctx.render_template_strict(f, "changelog footer", log)?;
        if !rendered.trim().is_empty() {
            final_markdown.push('\n');
            final_markdown.push_str(&rendered);
            final_markdown.push('\n');
            rendered_footer = Some(rendered);
        }
    }
    ctx.stage_outputs.changelog_header = rendered_header;
    ctx.stage_outputs.changelog_footer = rendered_footer;

    let dist = ctx.config.dist.clone();
    write_changelog_dist(log, &dist, &final_markdown)
}

/// Resolve the native pipeline's options: filters, groups, header/footer
/// content (post-`ContentSource::resolve`), abbrev, format template, and
/// pre-rendered path/title/divider templates.
fn resolve_changelog_opts(
    ctx: &mut Context,
    log: &StageLogger,
    cfg: Option<&ChangelogConfig>,
) -> Result<ChangelogOpts> {
    let sort_order = match cfg {
        Some(c) => c.resolved_sort()?.to_string(),
        None => String::new(),
    };
    let filters = cfg.and_then(|c| c.filters.as_ref());
    let exclude_filters: Vec<String> = filters.and_then(|f| f.exclude.clone()).unwrap_or_default();
    let include_filters: Vec<String> = filters.and_then(|f| f.include.clone()).unwrap_or_default();
    let groups: Vec<ChangelogGroup> = cfg.and_then(|c| c.groups.clone()).unwrap_or_default();

    let header_src: Option<ContentSource> = cfg.and_then(|c| c.header.clone());
    let footer_src: Option<ContentSource> = cfg.and_then(|c| c.footer.clone());
    // Resolve the ContentSource (Inline / FromFile / FromUrl) into a raw
    // string up front so the header/footer rendering pass below can stay
    // focused on template-expansion of the resulting body.
    let header: Option<String> = header_src
        .as_ref()
        .map(|src| anodizer_core::content_source::resolve(src, "changelog header", ctx))
        .transpose()?;
    let footer: Option<String> = footer_src
        .as_ref()
        .map(|src| anodizer_core::content_source::resolve(src, "changelog footer", ctx))
        .transpose()?;

    let abbrev: i32 = cfg
        .map(|c| c.resolved_abbrev())
        .unwrap_or(ChangelogConfig::DEFAULT_ABBREV);
    let format_template: Option<String> = cfg.and_then(|c| c.format.clone());
    let raw_paths: Vec<String> = cfg.and_then(|c| c.paths.clone()).unwrap_or_default();
    let raw_title: Option<String> = cfg.and_then(|c| c.title.clone());
    let raw_divider: Option<String> = cfg.and_then(|c| c.divider.clone());

    let paths: Vec<String> = raw_paths
        .into_iter()
        .map(|p| ctx.render_template_strict(&p, "changelog path", log))
        .collect::<Result<Vec<_>>>()?;
    let title = raw_title
        .map(|t| ctx.render_template_strict(&t, "changelog title", log))
        .transpose()?;
    let divider = raw_divider
        .map(|d| ctx.render_template_strict(&d, "changelog divider", log))
        .transpose()?;

    Ok(ChangelogOpts {
        sort_order,
        exclude_filters,
        include_filters,
        groups,
        header,
        footer,
        abbrev,
        format_template,
        paths,
        title,
        divider,
    })
}

/// Pick the effective path filter for a crate. Precedence:
/// changelog-level `paths` > per-crate `path` > monorepo dir.
fn effective_paths(
    changelog_paths: &[String],
    crate_path: &str,
    monorepo_dir: Option<&str>,
) -> Vec<String> {
    if !changelog_paths.is_empty() {
        changelog_paths.to_vec()
    } else if !crate_path.is_empty() && crate_path != "." {
        vec![crate_path.to_string()]
    } else if let Some(dir) = monorepo_dir {
        vec![dir.to_string()]
    } else {
        Vec::new()
    }
}

/// Fetch commits for a crate via the configured SCM backend, with
/// fallback-to-git on transient SCM API failures (strict mode escalates
/// to an error). Returns `(commits, logins_str)`.
fn fetch_crate_commits(
    ctx: &mut Context,
    log: &StageLogger,
    use_source: &str,
    prev_tag: &Option<String>,
    paths: &[String],
    crate_name: &str,
) -> Result<(Vec<CommitInfo>, String)> {
    let use_github = use_source == "github";
    let use_gitlab = use_source == "gitlab";
    let use_gitea = use_source == "gitea";

    // Pre-empt the SCM API call when there is no previous tag (first
    // release on a branch). GoReleaser's `getChangeloger` does the
    // same: it warns and returns the git changeloger directly.
    let scm_no_prev_tag = should_preempt_scm_to_git(use_github, use_gitlab, use_gitea, prev_tag);
    if scm_no_prev_tag {
        let scm_label = if use_github {
            "github"
        } else if use_gitlab {
            "gitlab"
        } else {
            "gitea"
        };
        log.status(&format!(
            "no previous tag found — using 'git' instead of '{}' for crate '{}'",
            scm_label, crate_name
        ));
    }

    if scm_no_prev_tag {
        return Ok((
            fetch_git_commits(prev_tag, paths, crate_name, log)?,
            String::new(),
        ));
    }
    if use_github {
        match fetch_github_commits(ctx, prev_tag, paths, log) {
            Ok((infos, logins)) => return Ok((infos, logins)),
            Err(e) => {
                ctx.strict_guard(
                    log,
                    &format!(
                        "changelog: GitHub API fetch failed, falling back to git: {}",
                        e
                    ),
                )?;
                return Ok((
                    fetch_git_commits(prev_tag, paths, crate_name, log)?,
                    String::new(),
                ));
            }
        }
    }
    if use_gitlab {
        match fetch_gitlab_commits(ctx, prev_tag, log) {
            Ok((infos, logins)) => return Ok((infos, logins)),
            Err(e) => {
                ctx.strict_guard(
                    log,
                    &format!(
                        "changelog: GitLab API fetch failed, falling back to git: {}",
                        e
                    ),
                )?;
                return Ok((
                    fetch_git_commits(prev_tag, paths, crate_name, log)?,
                    String::new(),
                ));
            }
        }
    }
    if use_gitea {
        match fetch_gitea_commits(ctx, prev_tag, log) {
            Ok((infos, logins)) => return Ok((infos, logins)),
            Err(e) => {
                ctx.strict_guard(
                    log,
                    &format!(
                        "changelog: Gitea API fetch failed, falling back to git: {}",
                        e
                    ),
                )?;
                return Ok((
                    fetch_git_commits(prev_tag, paths, crate_name, log)?,
                    String::new(),
                ));
            }
        }
    }
    Ok((
        fetch_git_commits(prev_tag, paths, crate_name, log)?,
        String::new(),
    ))
}

/// Run the per-crate fetch → filter → sort → group → render pipeline
/// and return the rendered Markdown for that crate.
fn render_crate_changelog(
    ctx: &mut Context,
    log: &StageLogger,
    crate_cfg: &anodizer_core::config::CrateConfig,
    opts: &ChangelogOpts,
    use_source: &str,
) -> Result<String> {
    let crate_name = crate_cfg.name.clone();
    let use_github = use_source == "github";
    let use_gitlab = use_source == "gitlab";
    let use_gitea = use_source == "gitea";

    // Find the previous tag for this crate, excluding the current tag
    // (otherwise the "latest matching tag" IS the current tag and the
    // commit range collapses to zero).
    let monorepo_prefix = ctx.config.monorepo_tag_prefix();
    let current_tag = ctx.template_vars().get("Tag").cloned();
    let prev_tag = find_latest_tag_matching_with_prefix(
        &crate_cfg.tag_template,
        ctx.config.git.as_ref(),
        Some(ctx.template_vars()),
        monorepo_prefix,
    )
    .unwrap_or(None)
    .filter(|t| current_tag.as_deref() != Some(t.as_str()));

    let monorepo_dir = ctx.config.monorepo_dir().map(str::to_string);
    let paths = effective_paths(&opts.paths, &crate_cfg.path, monorepo_dir.as_deref());

    // The GitHub API only supports filtering by a single path parameter;
    // GitLab/Gitea compare APIs don't support path filtering at all.
    if use_github && paths.len() > 1 {
        log.warn(&format!(
            "changelog: GitHub API only supports a single path filter; \
             only the first of {} paths ('{}') will be used for API queries. \
             Use `use: git` for accurate multi-path filtering.",
            paths.len(),
            paths[0]
        ));
    }
    if (use_gitlab || use_gitea) && !paths.is_empty() {
        log.warn(&format!(
            "changelog: {} API does not support path filtering; \
             {} path(s) will be ignored. Use `use: git` for path-based filtering.",
            if use_gitlab { "GitLab" } else { "Gitea" },
            paths.len()
        ));
    }

    let (all_commit_infos, logins_str) =
        fetch_crate_commits(ctx, log, use_source, &prev_tag, &paths, &crate_name)?;

    // GoReleaser treats include and exclude as mutually exclusive:
    // if include patterns are configured, exclude is completely ignored.
    let filtered = if !opts.include_filters.is_empty() {
        apply_include_filters(&all_commit_infos, &opts.include_filters, log)?
    } else {
        apply_filters(&all_commit_infos, &opts.exclude_filters, log)?
    };

    let mut sorted = filtered;
    sort_commits(&mut sorted, &opts.sort_order)?;

    let grouped = if opts.groups.is_empty() {
        // No groups configured — render commits as a flat list without
        // any group heading. GoReleaser only emits a "## Changes"
        // heading when groups ARE configured (for the "others" bucket);
        // with no groups the changelog is a plain bullet list.
        if sorted.is_empty() {
            vec![]
        } else {
            vec![GroupedCommits {
                title: String::new(),
                commits: sorted,
                subgroups: Vec::new(),
            }]
        }
    } else {
        group_commits(&sorted, &opts.groups, log)?
    };

    let scm_provider = ctx.token_type.to_string();
    render_changelog_with_provider(
        &grouped,
        ChangelogRenderOpts {
            abbrev: opts.abbrev,
            format_template: opts.format_template.as_deref(),
            logins: &logins_str,
            use_source,
            title: opts.title.as_deref(),
            divider: opts.divider.as_deref(),
            scm_provider: Some(&scm_provider),
        },
    )
}

/// Render header/footer through the template engine and wrap the
/// combined per-crate body. Stashes the rendered header/footer on
/// `ctx.stage_outputs` so the release stage can re-use them when no
/// `release.header` / `release.footer` override is configured (mirrors
/// GoReleaser's `loadContent(ReleaseHeader…)` flow).
fn wrap_with_header_footer(
    ctx: &mut Context,
    log: &StageLogger,
    body: &str,
    header: Option<&str>,
    footer: Option<&str>,
) -> Result<String> {
    let mut final_markdown = String::new();
    let mut rendered_header: Option<String> = None;
    if let Some(h) = header {
        let rendered = ctx.render_template_strict(h, "changelog header", log)?;
        if rendered.trim().is_empty() {
            log.warn("changelog: header rendered to an empty string (will be omitted)");
        } else {
            final_markdown.push_str(&rendered);
            final_markdown.push_str("\n\n");
            rendered_header = Some(rendered);
        }
    }
    final_markdown.push_str(body);
    let mut rendered_footer: Option<String> = None;
    if let Some(f) = footer {
        let rendered = ctx.render_template_strict(f, "changelog footer", log)?;
        if rendered.trim().is_empty() {
            log.warn("changelog: footer rendered to an empty string (will be omitted)");
        } else {
            final_markdown.push('\n');
            final_markdown.push_str(&rendered);
            final_markdown.push('\n');
            rendered_footer = Some(rendered);
        }
    }
    ctx.stage_outputs.changelog_header = rendered_header;
    ctx.stage_outputs.changelog_footer = rendered_footer;
    Ok(final_markdown)
}

/// Write the final Markdown to `<dist>/CHANGELOG.md`, creating the
/// directory first. GoReleaser writes this file even in dry-run mode.
fn write_changelog_dist(log: &StageLogger, dist: &PathBuf, markdown: &str) -> Result<()> {
    std::fs::create_dir_all(dist)
        .with_context(|| format!("changelog: create dist dir {}", dist.display()))?;
    let notes_path = dist.join("CHANGELOG.md");
    std::fs::write(&notes_path, markdown)
        .with_context(|| format!("changelog: write {}", notes_path.display()))?;
    log.status(&format!("wrote {}", notes_path.display()));
    Ok(())
}
