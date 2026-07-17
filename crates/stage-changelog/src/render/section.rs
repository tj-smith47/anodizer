use super::*;

// ---------------------------------------------------------------------------
// Public render API — produces a single staged changelog edit that can be
// bundled alongside a version bump in one commit.
// ---------------------------------------------------------------------------

/// Strategy describing how `ChangelogUpdate.rendered_text` relates to the
/// existing contents of `ChangelogUpdate.file_path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertionMode {
    /// `rendered_text` is the complete final file content; the caller should
    /// overwrite `file_path` with it (creating the file if missing).
    Replace,
}

/// A pending edit to a single changelog file.
#[derive(Debug, Clone)]
pub struct ChangelogUpdate {
    /// Absolute path of the changelog file the caller should write.
    pub file_path: std::path::PathBuf,
    /// Content the caller should write to `file_path`.
    pub rendered_text: String,
    /// How `rendered_text` should be applied at `file_path`.
    pub insertion_mode: InsertionMode,
}

/// Load the `changelog:` block from the workspace's anodizer config,
/// discovered via the shared well-known-name candidate list
/// ([`anodizer_core::config::find_config_candidate_in`] — the same set
/// every CLI command honors, so a repo using `anodizer.yaml` doesn't get a
/// silently-degraded changelog).
///
/// Returns `Ok(None)` when no config exists or it carries no `changelog:`
/// section. Deliberately skips the include/deprecation machinery in
/// `cli::pipeline::load_config` so this stays usable from non-CLI contexts
/// (core is already in the dep graph; pulling cli in would create a cycle).
pub(crate) fn load_changelog_config(
    workspace_root: &std::path::Path,
) -> Result<Option<anodizer_core::config::ChangelogConfig>> {
    let Some(cfg_path) = anodizer_core::config::find_config_candidate_in(workspace_root) else {
        return Ok(None);
    };
    let raw = anodizer_core::config::load_raw_config_value(&cfg_path)?;
    let changelog_yaml = match raw.get("changelog") {
        Some(v) => v.clone(),
        None => return Ok(None),
    };
    let cfg: anodizer_core::config::ChangelogConfig = serde_yaml_ng::from_value(changelog_yaml)
        .with_context(|| {
            format!(
                "failed to deserialize changelog config from {}",
                cfg_path.display()
            )
        })?;
    Ok(Some(cfg))
}

/// The scoping inputs read from the anodizer config beyond the `changelog`
/// block: every declared crate's directory and the optional `monorepo.dir`,
/// used to build the aggregate union in [`anodizer_core::changelog_scope`].
#[derive(Default)]
pub struct ScopeInputs {
    /// Every declared crate's `path` (top-level `crates:` and any nested
    /// `workspaces[].crates`).
    pub crate_dirs: Vec<String>,
    /// The optional `monorepo.dir`, the aggregate fallback when no crate
    /// directories are declared.
    pub monorepo_dir: Option<String>,
}

/// Read the crate directories and `monorepo.dir` from the discovered
/// anodizer config (shared well-known-name candidate list) so every
/// changelog format builds the aggregate scope from the SAME crate-dir source
/// (the engine-backed `keep-a-changelog`/`json` formats and the `release-notes`
/// stage both call this), guaranteeing they cannot drift.
///
/// A lightweight read of just `crates[].path` and `monorepo.dir` — the engine
/// crate cannot depend on the full CLI config loader, and scoping only needs
/// these two fields. Missing / empty returns an empty [`ScopeInputs`] (the
/// resolver then falls back to the whole-repo aggregate).
pub fn load_scope_inputs(workspace_root: &std::path::Path) -> Result<ScopeInputs> {
    let Some(cfg_path) = anodizer_core::config::find_config_candidate_in(workspace_root) else {
        return Ok(ScopeInputs::default());
    };
    let raw = anodizer_core::config::load_raw_config_value(&cfg_path)?;

    let mut crate_dirs: Vec<String> = Vec::new();
    if let Some(crates) = raw.get("crates").and_then(|c| c.as_sequence()) {
        for c in crates {
            if let Some(path) = c.get("path").and_then(|p| p.as_str()) {
                crate_dirs.push(path.to_string());
            }
        }
    }
    // `workspaces:`-style monorepo configs nest crates under each workspace.
    if let Some(workspaces) = raw.get("workspaces").and_then(|w| w.as_sequence()) {
        for ws in workspaces {
            if let Some(crates) = ws.get("crates").and_then(|c| c.as_sequence()) {
                for c in crates {
                    if let Some(path) = c.get("path").and_then(|p| p.as_str()) {
                        crate_dirs.push(path.to_string());
                    }
                }
            }
        }
    }

    let monorepo_dir = raw
        .get("monorepo")
        .and_then(|m| m.get("dir"))
        .and_then(|d| d.as_str())
        .map(str::to_string);

    Ok(ScopeInputs {
        crate_dirs,
        monorepo_dir,
    })
}

/// The per-crate template inputs the write path resolves templated group
/// fields (`groups[].title` / `divider` / `paths`) against.
///
/// The in-pipeline `Stage::run` path renders these through the full `Context`;
/// the write path (`bump`/`tag` changelog sync) has no `Context`, so it builds
/// a minimal [`TemplateVars`] from these inputs so all three output formats
/// (per-crate Markdown, root Markdown, JSON) agree byte-for-byte. `version` and
/// `tag` are empty on the `[Unreleased]` refresh path (no release version yet).
#[derive(Clone, Copy)]
pub(crate) struct SectionVars<'a> {
    pub(crate) crate_name: &'a str,
    pub(crate) version: &'a str,
    pub(crate) tag: &'a str,
}

/// Build the template context the write path renders templated group fields
/// against, mirroring the per-crate vars [`anodizer_core::crate_scope`]
/// installs into the `Context` (`ProjectName` / `Name` / `Version` /
/// `RawVersion` / `Tag`).
pub(crate) fn build_section_template_vars(sv: SectionVars<'_>) -> TemplateVars {
    let mut vars = TemplateVars::new();
    vars.set("ProjectName", sv.crate_name);
    vars.set("Name", sv.crate_name);
    vars.set("Version", sv.version);
    vars.set("RawVersion", sv.version);
    vars.set("Tag", sv.tag);
    vars
}

/// Render a single templated field through `vars`, falling back to the raw
/// string on a render error.
///
/// The write path (`bump`/`tag` changelog sync) has no `Context`, so it can't
/// consult `--strict`; a malformed template is kept verbatim (matching the
/// non-strict `render_template_strict` branch) rather than failing the
/// `bump`/`tag` mid-flight. But the fallback must never be SILENT: a `log.warn`
/// naming the field kind and the offending template surfaces the defect at
/// default verbosity so a literal `{{ … }}` is never committed into
/// `CHANGELOG.md` without a trace.
pub(crate) fn render_field(
    raw: &str,
    vars: &TemplateVars,
    field: &str,
    log: &StageLogger,
) -> String {
    template::render(raw, vars).unwrap_or_else(|e| {
        log.warn(&format!(
            "changelog write path: failed to render templated {field} {raw:?}: {e}; keeping it verbatim"
        ));
        raw.to_string()
    })
}

/// Recursively render each group's `title` through `vars`, mutating the tree
/// in place so the grouping/render pipeline downstream sees resolved headings.
pub(crate) fn render_group_titles_in_place(
    groups: &mut [ChangelogGroup],
    vars: &TemplateVars,
    log: &StageLogger,
) {
    for g in groups.iter_mut() {
        if !g.title.is_empty() {
            g.title = render_field(&g.title, vars, "group title", log);
        }
        if let Some(subs) = g.groups.as_mut() {
            render_group_titles_in_place(subs, vars, log);
        }
    }
}

/// Render the config's templated group fields (`groups[].title`, `divider`,
/// `paths`) through `vars` so the write path matches the in-pipeline
/// `Stage::run` rendering. Mutates `cfg` in place.
pub(crate) fn render_section_config_fields(
    cfg: &mut anodizer_core::config::ChangelogConfig,
    vars: &TemplateVars,
    log: &StageLogger,
) {
    if let Some(groups) = cfg.groups.as_mut() {
        render_group_titles_in_place(groups, vars, log);
    }
    if let Some(divider) = cfg.divider.as_ref() {
        cfg.divider = Some(render_field(divider, vars, "divider", log));
    }
    if let Some(paths) = cfg.paths.as_ref() {
        cfg.paths = Some(
            paths
                .iter()
                .map(|p| render_field(p, vars, "path", log))
                .collect(),
        );
    }
}

/// Load the crate's changelog config, fetch path-filtered commits in the
/// range `from_tag..to_ref`, then filter/sort/group them into a
/// [`GroupedCommits`] tree (no Markdown rendering).
///
/// Single-sources the config-load + commit-fetch + group pipeline shared by
/// every public entry point (the Markdown promote/refresh paths and the JSON
/// renderer) so they can never drift on which commits they see.
///
/// `to_ref` bounds the upper end of the commit range (`None` ⇒ `HEAD`).
/// `section_vars` supplies the per-crate template context used to resolve
/// templated `groups[].title` / `divider` / `paths` (so the write path renders
/// them the same way the in-pipeline `Stage::run` does).
///
/// Returns the resolved [`ChangelogConfig`] alongside the grouped tree.
/// Returns `Ok(None)` under the conditions every caller treats as "nothing to
/// release": no anodizer config is present / it has no `changelog:` block, or
/// there are no qualifying commits in range (after filtering/grouping).
pub(crate) fn group_section_commits(
    workspace_root: &std::path::Path,
    crate_path: &std::path::Path,
    from_tag: Option<&str>,
    to_ref: Option<&str>,
    section_vars: SectionVars<'_>,
) -> Result<Option<(anodizer_core::config::ChangelogConfig, Vec<GroupedCommits>)>> {
    let Some(mut cfg) = load_changelog_config(workspace_root)? else {
        return Ok(None);
    };

    let log = StageLogger::new("bump-changelog", Verbosity::default());

    // Resolve templated group fields (`groups[].title` / `divider` / `paths`)
    // against the per-crate context before they feed grouping/rendering, so the
    // write path matches the in-pipeline `Stage::run` rendering and never ships
    // a literal `{{ ... }}` into the committed CHANGELOG.md.
    let template_vars = build_section_template_vars(section_vars);
    render_section_config_fields(&mut cfg, &template_vars, &log);

    // The current track's directory relative to the workspace root; empty for
    // the aggregate (the workspace-root "crate"), exactly as the resolver
    // expects.
    let rel_crate_path = relative_filter(workspace_root, crate_path).unwrap_or_default();
    let scope_inputs = load_scope_inputs(workspace_root)?;
    let changelog_paths: Vec<String> = cfg.paths.clone().unwrap_or_default();
    let scope = anodizer_core::changelog_scope::resolve_changelog_scope(
        &rel_crate_path,
        &scope_inputs.crate_dirs,
        scope_inputs.monorepo_dir.as_deref(),
        &changelog_paths,
    );

    // The git pathspec scope is applied via `--`; a precise `changelog.paths`
    // narrowing (when one is required) is intersected over the fetched commits'
    // touched files so all three formats agree byte-for-byte.
    let raw_commits = if scope.narrow.is_some() {
        let pairs = crate::fetch::fetch_git_commits_with_files_in(
            workspace_root,
            from_tag,
            to_ref,
            scope.pathspecs(),
        )?;
        pairs
            .into_iter()
            .filter(|p| scope.commit_survives_narrow(&p.files))
            .map(|p| p.commit)
            .collect::<Vec<_>>()
    } else {
        crate::fetch::fetch_git_commits_in_paths(
            workspace_root,
            from_tag,
            to_ref,
            scope.pathspecs(),
        )?
    };
    if raw_commits.is_empty() {
        return Ok(None);
    }

    let mut infos: Vec<CommitInfo> = raw_commits
        .iter()
        .map(|c| {
            let mut info = parse_commit_message(&c.message);
            info.hash = c.short_hash.clone();
            info.full_hash = c.hash.clone();
            info.author_name = c.author_name.clone();
            info.author_email = c.author_email.clone();
            info.co_authors = extract_co_authors(&c.body);
            info
        })
        .collect();

    let include: Vec<String> = cfg
        .filters
        .as_ref()
        .and_then(|f| f.include.clone())
        .unwrap_or_default();
    infos = if !include.is_empty() {
        apply_include_filters(&infos, &include, &log)?
    } else {
        // Shared with the release-notes path: user excludes + the version-sync
        // bump auto-exclude. Previously this path re-derived only the raw
        // `filters.exclude`, so the default `keep-a-changelog` / `json` formats
        // and the committed CHANGELOG.md leaked anodizer's own bump commits.
        let exclude = exclude_filters_with_version_sync(cfg.filters.as_ref());
        apply_filters(&infos, &exclude, &log)?
    };

    sort_commits(&mut infos, cfg.resolved_sort()?)?;

    // GitHub login enrichment for the on-disk changelog: resolve author
    // emails to `@login` mentions when the release targets GitHub AND a
    // token resolves through the standard chain (`bump`/`tag`/`changelog`
    // expose no `--token` flag, so explicit is `None` here and the env links
    // `ANODIZER_GITHUB_TOKEN` → `GITHUB_TOKEN` carry the chain; no token →
    // name-based rendering, by contract). The per-call enricher is cheap —
    // the email→login memo is process-wide in core, so a multi-crate
    // `bump`/`tag` sync still costs one API call per unique author email.
    // Failures keep name-based rendering.
    if crate::enrich::use_source_supports_github_logins(cfg.resolved_use_source())
        && let Some(token) = anodizer_core::git::resolve_github_token(None)
    {
        let configured = crate::enrich::configured_github_target(workspace_root);
        if let Some((owner, repo)) = crate::enrich::derive_github_target(
            configured.as_ref().map(|(o, n)| (o.as_str(), n.as_str())),
            workspace_root,
        ) {
            crate::enrich::LoginEnricher::for_github_repo(owner, repo, token, workspace_root)
                .enrich(&mut infos);
        }
    }

    let groups: Vec<ChangelogGroup> = cfg.groups.clone().unwrap_or_default();
    let grouped = if groups.is_empty() {
        if infos.is_empty() {
            Vec::new()
        } else {
            vec![GroupedCommits {
                title: String::new(),
                commits: infos,
                subgroups: Vec::new(),
            }]
        }
    } else {
        group_commits(&infos, &groups, &log)?
    };

    if grouped.is_empty() {
        return Ok(None);
    }

    Ok(Some((cfg, grouped)))
}

/// Load config, fetch+group commits for `from_tag..to_ref`, and render the
/// grouped commit body (`### <GroupTitle>` group headings, no `## <version>`
/// heading).
///
/// Thin Markdown wrapper over [`group_section_commits`]; returns the resolved
/// [`ChangelogConfig`] alongside the rendered body. `Ok(None)` propagates the
/// same "nothing to release" conditions.
pub(crate) fn render_section_body(
    workspace_root: &std::path::Path,
    crate_path: &std::path::Path,
    from_tag: Option<&str>,
    to_ref: Option<&str>,
    section_vars: SectionVars<'_>,
) -> Result<Option<(anodizer_core::config::ChangelogConfig, String)>> {
    let Some((cfg, grouped)) =
        group_section_commits(workspace_root, crate_path, from_tag, to_ref, section_vars)?
    else {
        return Ok(None);
    };

    let abbrev = cfg.resolved_abbrev();
    let body = render_changelog_with_provider(
        &grouped,
        ChangelogRenderOpts {
            abbrev,
            format_template: cfg.format.as_deref(),
            logins: "",
            use_source: cfg.resolved_use_source(),
            title: Some(""),
            divider: cfg.divider.as_deref(),
            scm_provider: None,
            // On-disk Markdown gets no GitHub autolinking, so resolved
            // logins render as explicit links.
            login_style: LoginStyle::Linked,
        },
    )?;
    // `render_changelog_with_provider` always emits a `## <title>` line,
    // suppressed here by passing `Some("")`, which produces `## \n\n`. Drop
    // any empty heading at the start so the caller's `## [<version>]` heading
    // stands alone.
    let body = body.trim_start_matches("## \n\n").trim_start().to_string();

    Ok(Some((cfg, body)))
}

/// One commit in the JSON changelog DTO.
#[derive(serde::Serialize)]
pub(crate) struct JsonEntry {
    /// Commit subject / description (the conventional-commit body when parsed).
    summary: String,
    /// Abbreviated commit hash.
    sha: String,
    /// Full 40-char commit hash.
    full_sha: String,
    /// Primary author plus any `Co-Authored-By:` trailer names.
    authors: Vec<String>,
}

/// One commit group (and its nested subgroups) in the JSON changelog DTO.
#[derive(serde::Serialize)]
pub(crate) struct JsonGroup {
    /// Group heading (empty when no `groups:` are configured).
    title: String,
    /// Commits bucketed directly under this group.
    entries: Vec<JsonEntry>,
    /// Nested subgroups, mirroring the configured `groups[].groups`.
    subgroups: Vec<JsonGroup>,
}

/// Top-level JSON changelog DTO for a single commit range.
#[derive(serde::Serialize)]
pub(crate) struct JsonChangelog {
    /// Lower bound of the range (the `from_tag`), or `null` for full history.
    from: Option<String>,
    /// Resolved upper bound of the range (`HEAD` when unbounded).
    to: String,
    /// Grouped commits in render order.
    groups: Vec<JsonGroup>,
}

/// Map an internal [`GroupedCommits`] node to its public JSON DTO, recursing
/// into subgroups. Deliberately projects only the stable public fields so the
/// internal commit/group shapes can evolve without breaking the JSON contract.
pub(crate) fn group_to_json(group: &GroupedCommits) -> JsonGroup {
    let entries = group
        .commits
        .iter()
        .map(|c| {
            let mut authors: Vec<String> = Vec::new();
            if !c.author_name.is_empty() {
                authors.push(c.author_name.clone());
            }
            authors.extend(c.co_authors.iter().filter(|a| !a.is_empty()).cloned());
            JsonEntry {
                summary: c.description.clone(),
                sha: c.hash.clone(),
                full_sha: c.full_hash.clone(),
                authors,
            }
        })
        .collect();
    JsonGroup {
        title: group.title.clone(),
        entries,
        subgroups: group.subgroups.iter().map(group_to_json).collect(),
    }
}

/// Serialize the grouped commits for `from_tag..to_ref` to pretty-printed JSON.
///
/// Reuses the shared fetch + filter + group pipeline, then projects the tree
/// onto a stable public DTO (`{ from, to, groups: [{ title, entries: [{
/// summary, sha, full_sha, authors }], subgroups }] }`). `from` is the
/// `from_tag` or `null`; `to` is the resolved upper bound (`HEAD` when
/// `to_ref` is `None`).
///
/// Returns `Ok(None)` when there is no `changelog:` config or no qualifying
/// commits in range (matching the Markdown entry points' "nothing to render"
/// signal).
pub fn render_changelog_json(
    workspace_root: &std::path::Path,
    crate_path: &std::path::Path,
    from_tag: Option<&str>,
    to_ref: Option<&str>,
) -> Result<Option<String>> {
    // The JSON preview renders an arbitrary range with no release version/tag
    // in hand, so only `ProjectName`/`Name` carry meaning for a templated group
    // title; derive them from the crate directory name (a group title
    // referencing `{{ .Version }}`/`{{ .Tag }}` resolves to empty).
    let crate_name = crate_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let section_vars = SectionVars {
        crate_name,
        version: "",
        tag: "",
    };
    let Some((_cfg, grouped)) =
        group_section_commits(workspace_root, crate_path, from_tag, to_ref, section_vars)?
    else {
        return Ok(None);
    };

    let dto = JsonChangelog {
        from: from_tag.map(str::to_string),
        to: to_ref.unwrap_or("HEAD").to_string(),
        groups: grouped.iter().map(group_to_json).collect(),
    };

    let json = serde_json::to_string_pretty(&dto)
        .with_context(|| "changelog: serialize JSON changelog DTO".to_string())?;
    Ok(Some(json))
}
