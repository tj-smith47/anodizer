use anodizer_core::config::ChangelogGroup;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::Result;

use super::{ChangelogRenderOpts, render_changelog_with_provider};
use crate::fetch::fetch_crate_commits;
use crate::group::{
    GroupedCommits, apply_filters, apply_include_filters, group_commits, sort_commits,
};
use crate::run::{ChangelogOpts, resolve_prev_tag};

/// Recursively render each `ChangelogGroup.title` through the project's
/// template context. Templated group headings are accepted (e.g.
/// `title: "{{ .ProjectName }} features"`); rendering at the stage edge
/// keeps `render_groups` free of template-engine ceremony.
pub(crate) fn render_group_titles(
    ctx: &mut Context,
    log: &StageLogger,
    groups: Vec<ChangelogGroup>,
) -> Result<Vec<ChangelogGroup>> {
    let mut out = Vec::with_capacity(groups.len());
    for g in groups {
        let rendered_title = if g.title.is_empty() {
            String::new()
        } else {
            ctx.render_template_strict(&g.title, "changelog group title", log)?
        };
        let rendered_subgroups = match g.groups {
            Some(subs) => Some(render_group_titles(ctx, log, subs)?),
            None => None,
        };
        out.push(ChangelogGroup {
            title: rendered_title,
            regexp: g.regexp,
            order: g.order,
            groups: rendered_subgroups,
        });
    }
    Ok(out)
}

/// Resolve the commit path-scope for `crate_cfg` via the shared
/// [`anodizer_core::changelog_scope`] resolver — the single source of truth
/// every changelog format routes through. A per-crate track scopes to its own
/// directory; the aggregate to the union of all crate dirs + manifests (or the
/// monorepo dir / whole repo). `changelog.paths`, when set, can only NARROW
/// the derived scope, never replace it.
fn resolve_scope(
    changelog_paths: &[String],
    crate_path: &str,
    all_crate_dirs: &[String],
    monorepo_dir: Option<&str>,
) -> anodizer_core::changelog_scope::ChangelogScope {
    anodizer_core::changelog_scope::resolve_changelog_scope(
        crate_path,
        all_crate_dirs,
        monorepo_dir,
        changelog_paths,
    )
}

/// Run the per-crate fetch → filter → sort → group → render pipeline
/// and return the rendered Markdown for that crate.
pub(crate) fn render_crate_changelog(
    ctx: &mut Context,
    log: &StageLogger,
    crate_cfg: &anodizer_core::config::CrateConfig,
    opts: &ChangelogOpts,
    use_source: &str,
    enricher: &mut Option<crate::enrich::LoginEnricher<'static>>,
) -> Result<String> {
    let crate_name = crate_cfg.name.clone();
    let use_github = use_source == "github";
    let use_gitlab = use_source == "gitlab";
    let use_gitea = use_source == "gitea";

    // Find the previous tag for this crate, excluding the current tag
    // (otherwise the "latest matching tag" IS the current tag and the
    // commit range collapses to zero). An explicit `--from <ref>` overrides
    // the auto-discovered tag as the range start.
    let monorepo_prefix = ctx.config.monorepo_tag_prefix();
    let current_tag = ctx.template_vars().get("Tag").cloned();
    let prev_tag = resolve_prev_tag(ctx, crate_cfg, monorepo_prefix, current_tag.as_deref())?;

    // Source the aggregate's crate dirs + monorepo dir from `.anodizer.yaml` —
    // the SAME read the engine-backed kac/json formats use — so all three
    // formats build an identical aggregate union and cannot drift. The
    // standalone release-notes command flattens `ctx.config.crates` to one
    // path-cleared aggregate entry, so reading them off `ctx.config` would lose
    // the union; the on-disk read recovers it.
    let scope_root = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let scope_inputs = crate::render::load_scope_inputs(&scope_root)?;
    let monorepo_dir = scope_inputs
        .monorepo_dir
        .clone()
        .or_else(|| ctx.config.monorepo_dir().map(str::to_string));
    let scope = resolve_scope(
        &opts.paths,
        &crate_cfg.path,
        &scope_inputs.crate_dirs,
        monorepo_dir.as_deref(),
    );
    let paths = scope.pathspecs().to_vec();

    // The GitHub API only supports filtering by a single path parameter;
    // GitLab/Gitea compare APIs don't support path filtering at all.
    if use_github && paths.len() > 1 {
        log.warn(&format!(
            "GitHub API only supports a single path filter; \
             only the first of {} paths ('{}') will be used for API queries. \
             Use `use: git` for accurate multi-path filtering.",
            paths.len(),
            paths[0]
        ));
    }
    if (use_gitlab || use_gitea) && !paths.is_empty() {
        log.warn(&format!(
            "{} API does not support path filtering; \
             {} path(s) will be ignored. Use `use: git` for path-based filtering.",
            if use_gitlab { "GitLab" } else { "Gitea" },
            paths.len()
        ));
    }
    // A precise `changelog.paths` glob narrowing applies only to `use: git`
    // (the SCM compare APIs already warn that path filtering is coarse /
    // unsupported above). Surface the limitation rather than silently widen.
    if scope.narrow.is_some() && (use_github || use_gitlab || use_gitea) {
        log.warn(
            "`changelog.paths` narrows the per-crate scope, but the \
             SCM compare API cannot apply it precisely; the result may include \
             commits outside the configured paths. Use `use: git` for an exact \
             intersect.",
        );
    }

    // The explicit upper bound (`changelog <from>..<to>` / single tag): bounds
    // the commit walk at `<to>` instead of HEAD. `None` keeps the pending
    // window running to HEAD. Cloned before the `&mut ctx` borrow below.
    let changelog_to = ctx.options.changelog_to.clone();
    let (all_commit_infos, logins_str) = fetch_crate_commits(
        ctx,
        log,
        use_source,
        &prev_tag,
        changelog_to.as_deref(),
        &scope,
        &crate_name,
        &scope_root,
    )?;

    // include and exclude are mutually exclusive:
    // if include patterns are configured, exclude is completely ignored.
    let filtered = if !opts.include_filters.is_empty() {
        apply_include_filters(&all_commit_infos, &opts.include_filters, log)?
    } else {
        apply_filters(&all_commit_infos, &opts.exclude_filters, log)?
    };

    let mut sorted = filtered;
    sort_commits(&mut sorted, &opts.sort_order)?;

    // Resolve GitHub logins for commits that arrived without one (the
    // local-git backend, or SCM-API authors with no linked account). The
    // run-wide enricher memoizes per email, and unresolved commits keep
    // their byte-identical name-based rendering.
    if let Some(e) = enricher.as_mut() {
        e.enrich(&mut sorted);
    }

    let grouped = if opts.groups.is_empty() {
        // No groups configured — render commits as a flat list without
        // any group heading. Only a "## Changes" heading is emitted
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
            // The stage body becomes the GitHub release body, where bare
            // `@login` mentions are autolinked by GitHub itself.
            login_style: crate::render::LoginStyle::Bare,
        },
    )
}

/// Render header/footer through the template engine and wrap the
/// combined per-crate body. Stashes the rendered header/footer on
/// `ctx.stage_outputs` so the release stage can re-use them when no
/// `release.header` / `release.footer` override is configured (mirrors
/// the release-header content-loading flow).
pub(crate) fn wrap_with_header_footer(
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
            log.warn("header rendered to an empty string (will be omitted)");
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
            log.warn("footer rendered to an empty string (will be omitted)");
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
