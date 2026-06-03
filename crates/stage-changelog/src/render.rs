//! Markdown rendering and the `render_crate_section` public entry point.
//!
//! `render_changelog_with_provider` is the canonical render function
//! (used by both the in-pipeline `Stage::run` body and the
//! `bump --commit` flow in `render_crate_section`). The recursive
//! `render_groups` walks the `GroupedCommits` tree to produce headings +
//! bullets at the configured Markdown depth.
//!
//! `merge_into_changelog` is the file-level merge helper used by
//! `render_crate_section` to fold a new release into an existing
//! `CHANGELOG.md`. It detects the [Keep a Changelog] shape (a
//! `## [Unreleased]` heading): in that mode it promotes the
//! `## [Unreleased]` section to the released version, inserts a fresh
//! empty `## [Unreleased]`, and rolls the `[Unreleased]` / `[<version>]`
//! compare-link footer. Otherwise it falls back to splicing a new
//! `## [<version>]` section directly after the leading H1.
//!
//! [Keep a Changelog]: https://keepachangelog.com/

use anodizer_core::config::ChangelogGroup;
use anodizer_core::template::{self, TemplateVars};
use anyhow::{Context as _, Result};
use serde_json::Value as JsonValue;

use crate::fetch::{fetch_git_commits_in, relative_filter};
use crate::group::{
    CommitInfo, GroupedCommits, apply_filters, apply_include_filters, extract_co_authors,
    group_commits, parse_commit_message, sort_commits,
};

/// Per-call rendering options for [`render_changelog_with_provider`].
///
/// Bundles the long parameter list so the public render entry point keeps a
/// readable signature. All fields are borrowed; the struct is short-lived.
#[derive(Clone, Copy)]
pub(crate) struct ChangelogRenderOpts<'a> {
    pub abbrev: i32,
    pub format_template: Option<&'a str>,
    pub logins: &'a str,
    pub use_source: &'a str,
    pub title: Option<&'a str>,
    pub divider: Option<&'a str>,
    /// Overrides `use_source` for newline selection only (matches
    /// the backend token type).
    pub scm_provider: Option<&'a str>,
}

/// Compute the release-wide unique author-name set across every commit
/// in the rendered groups (including subgroups) and return it as a
/// comma-separated string. Mirrors how `AllLogins` is built upstream of
/// `render_changelog_with_provider`; surfaces as the `AllAuthors`
/// per-line template var so users can render
/// `Contributors: {{ AllAuthors }}` once at the bottom of a changelog
/// footer (the per-line scope is the only template scope anodizer
/// currently exposes from the changelog renderer).
fn collect_all_authors(grouped: &[GroupedCommits]) -> String {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    fn walk(group: &GroupedCommits, seen: &mut std::collections::BTreeSet<String>) {
        for commit in &group.commits {
            if !commit.author_name.is_empty() {
                seen.insert(commit.author_name.clone());
            }
            for ca in &commit.co_authors {
                if !ca.is_empty() {
                    seen.insert(ca.clone());
                }
            }
        }
        for sub in &group.subgroups {
            walk(sub, seen);
        }
    }
    for g in grouped {
        walk(g, &mut seen);
    }
    seen.into_iter().collect::<Vec<_>>().join(", ")
}

/// Inner render function that accepts an optional SCM provider override for
/// newline handling, keyed on the backend token type, not
/// the changelog source. When `scm_provider` is set, it overrides `use_source`
/// for newline selection (but not for default format template selection).
pub(crate) fn render_changelog_with_provider(
    grouped: &[GroupedCommits],
    opts: ChangelogRenderOpts<'_>,
) -> Result<String> {
    let ChangelogRenderOpts {
        abbrev,
        format_template,
        logins,
        use_source,
        title,
        divider,
        scm_provider,
    } = opts;
    use anodizer_core::config::ChangelogConfig;
    // Build a transient ChangelogConfig with just the user-supplied
    // format so resolved_format applies the same precedence the
    // ChangelogStage call site uses.
    let probe = ChangelogConfig {
        format: format_template.map(|s| s.to_string()),
        ..Default::default()
    };
    let tmpl: &str = probe.resolved_format(use_source, abbrev);
    // GitLab and Gitea need trailing spaces before newlines for markdown line breaks.
    // Newline handling is keyed on the backend token type, not the changelog source.
    // See https://docs.gitlab.com/ee/user/markdown.html#newlines
    let nl_source = scm_provider.unwrap_or(use_source);
    let newline = match nl_source {
        "gitlab" | "gitea" => "   \n",
        _ => "\n",
    };
    let mut out = String::new();
    // Title heading. Three states:
    //   - title == None        → emits `## Changelog` (default).
    //   - title == Some("foo") → emits `## foo`.
    //   - title == Some("")    → suppresses the heading entirely (anodize-additive
    //                            UX win — see divergence note below).
    //
    // Intentional carve-out: the upstream convention emits the
    // `## Changelog` heading unconditionally and offers no way to suppress it.
    // Anodizer treats an explicit empty `title:` as a request to omit the
    // heading, which lets users compose changelogs into other surfaces (release
    // bodies, RSS feeds, embedded docs) without a redundant heading. Default
    // the carve-out is opt-in.
    let changelog_title = title.unwrap_or(ChangelogConfig::DEFAULT_TITLE);
    if !changelog_title.is_empty() {
        out.push_str(&format!("## {}\n\n", changelog_title));
    }
    let all_authors = collect_all_authors(grouped);
    let state = RenderGroupsState {
        abbrev,
        tmpl,
        logins,
        all_authors: &all_authors,
        divider,
        newline,
    };
    render_groups(&mut out, grouped, &state, 3)?;
    Ok(out)
}

/// State shared across the recursive [`render_groups`] tree walk.
///
/// Bundles every parameter that's invariant across the recursion (only `out`,
/// `groups`, and `depth` change). `divider` is the single field that's
/// suppressed at subgroup boundaries — that's expressed by passing
/// `state.with_divider(None)` into the recursive call.
#[derive(Clone, Copy)]
struct RenderGroupsState<'a> {
    abbrev: i32,
    tmpl: &'a str,
    logins: &'a str,
    all_authors: &'a str,
    divider: Option<&'a str>,
    newline: &'a str,
}

impl<'a> RenderGroupsState<'a> {
    fn with_divider(self, divider: Option<&'a str>) -> Self {
        Self { divider, ..self }
    }
}

/// Recursively render grouped commits at the given heading depth.
/// Depth is capped at 6 (matching Markdown's `######` max heading level).
fn render_groups(
    out: &mut String,
    groups: &[GroupedCommits],
    state: &RenderGroupsState<'_>,
    depth: usize,
) -> Result<()> {
    if depth > 6 {
        return Ok(());
    }
    let hashes = "#".repeat(depth);
    for (i, group) in groups.iter().enumerate() {
        // Insert divider between groups (not before the first one).
        if i > 0
            && let Some(div) = state.divider
        {
            out.push_str(div);
            out.push('\n');
        }
        // Only emit a heading when the group has a non-empty title.
        // When no changelog groups are configured, the default group has an
        // empty title so commits render as a plain bullet list without a
        // spurious heading.
        if !group.title.is_empty() {
            // Group titles are pre-rendered by the stage before reaching
            // this function (`run.rs::resolve_changelog_opts` walks the
            // groups tree and resolves each title through the project's
            // template context). Heading-emit is therefore a plain
            // string append.
            out.push_str(&format!("{} {}\n\n", hashes, group.title));
        }
        for commit in &group.commits {
            render_commit_line(
                out,
                commit,
                state.abbrev,
                state.tmpl,
                state.logins,
                state.all_authors,
                state.newline,
            )?;
        }
        // Render nested subgroups one level deeper (no divider at subgroup level).
        if !group.subgroups.is_empty() {
            render_groups(out, &group.subgroups, &state.with_divider(None), depth + 1)?;
        }
        // Add trailing newline after commits. Skip if this group has subgroups
        // (they add their own spacing) and no direct commits.
        if !group.commits.is_empty() || group.subgroups.is_empty() {
            out.push('\n');
        }
    }
    Ok(())
}

/// Render a single commit as a bullet line.
///
/// Template variables available:
/// - `SHA` — full commit hash
/// - `ShortSHA` — abbreviated commit hash (controlled by `abbrev`)
/// - `Message` — commit subject / description
/// - `AuthorName` — commit author name. Marked
///   `AuthorName` / `AuthorEmail` / `AuthorUsername` as deprecated in
///   favour of `AuthorsList[0].Name` / `.Email` / `.Username`. Anodizer
///   keeps the flat fields for backward compatibility; new templates
///   should prefer the `AuthorsList` structured form.
/// - `AuthorEmail` — commit author email (see `AuthorName` deprecation note).
/// - `Login` — per-commit GitHub username (populated only with `github` backend)
/// - `Authors` — comma-separated names for this commit (primary author +
///   `Co-Authored-By:` trailers). The per-entry
///   Authors template var.
/// - `Logins` — comma-separated logins for this commit (primary author's
///   login + parsed co-author logins, where the trailer carries one).
///   The per-entry Logins template var.
/// - `AllLogins` — comma-separated list of *all* GitHub logins seen in the
///   release. Was the previous `Logins` semantic (release-wide) before
///   `Logins` was reclaimed for per-entry data; renamed to keep both
///   available without ambiguity.
/// - `AllAuthors` — comma-separated, alphabetically-sorted list of unique
///   commit + co-author names seen across the entire changelog window
///   (release-wide). Available on every per-commit line so a user can
///   render `Contributors: {{ AllAuthors }}` in the footer slot of
///   their format template; the value is identical for every line.
fn render_commit_line(
    out: &mut String,
    commit: &CommitInfo,
    abbrev: i32,
    tmpl: &str,
    logins: &str,
    all_authors: &str,
    newline: &str,
) -> Result<()> {
    let short_sha = if abbrev < 0 {
        // Negative abbrev (e.g. -1) means omit hash entirely.
        String::new()
    } else if abbrev == 0 {
        // abbrev 0 means full SHA (no truncation).
        commit.full_hash.clone()
    } else {
        // Truncate the 40-char `full_hash`. `commit.hash` is already
        // git's `%h` short form (~7 chars) and so cannot honor any
        // abbrev > ~7 without falling back to a shorter value than
        // the user requested. abbrev: 12 (a common config) must yield
        // a 12-char SHA, not a silent 7-char one.
        //
        // `get(..a)` is byte-bounds-safe — a panicking `[..a]` slice
        // would otherwise blow up the github-native fallback path
        // (`info.full_hash = sha.to_string()` is set only when a SHA
        // is present; an empty default stays `""` from Default).
        let a = abbrev as usize;
        commit
            .full_hash
            .get(..a)
            .map(|s| s.to_string())
            .unwrap_or_else(|| commit.full_hash.clone())
    };
    let mut vars = TemplateVars::new();
    // SHA respects the `abbrev` config.
    // `short_sha` is already computed with abbrev applied above; use it here
    // so templates referencing {{ .SHA }} honor the user's abbreviation.
    vars.set("SHA", &short_sha);
    vars.set("ShortSHA", &short_sha);
    vars.set("Message", &commit.description);
    vars.set("AuthorName", &commit.author_name);
    vars.set("AuthorEmail", &commit.author_email);
    vars.set("Login", &commit.login);
    // Alias: the default format string when
    // `use ∈ {github,gitlab,gitea}` is
    // `"{{ .SHA }}: {{ .Message }} ({{ with .AuthorUsername }}@{{ . }}{{ else }}{{ .AuthorName }} <{{ .AuthorEmail }}>{{ end }})"`
    // populated for
    // `AuthorUsername` template var from the SCM commit author's username;
    // anodizer surfaces the same datum under `Login`. Bind both keys so
    // configs copy-paste cleanly without a "missing key" error.
    vars.set("AuthorUsername", &commit.login);
    // Per-entry `Authors` and `Logins` template vars: each entry gets its
    // own commit-author + co-author list. The release-wide GitHub login
    // list lives under `AllLogins` so `Logins` can carry the per-commit
    // semantic. Co-author entries (parsed from `Co-Authored-By:` trailers)
    // carry both bare name and "Name <email>" form; we surface their raw
    // trailer payload as the Authors join target.
    //
    // `Authors` (comma-string, backward-compatible) and `AuthorsList`
    // (structured {Name, Email, Username} records for
    // `{% for a in AuthorsList %}@{{ a.Username }}{% endfor %}`) are built
    // from a SINGLE walk over the author + co-author set: the comma-string
    // is derived from the structured list's names rather than re-iterating.
    // Co-author trailers contribute Name only (email is in the raw trailer
    // string; Username is unknown without an extra SCM API hit — left empty).
    let mut authors_list: Vec<JsonValue> = Vec::new();
    if !commit.author_name.is_empty() {
        let mut obj = serde_json::Map::new();
        obj.insert("Name".into(), JsonValue::String(commit.author_name.clone()));
        obj.insert(
            "Email".into(),
            JsonValue::String(commit.author_email.clone()),
        );
        obj.insert("Username".into(), JsonValue::String(commit.login.clone()));
        authors_list.push(JsonValue::Object(obj));
    }
    for ca in &commit.co_authors {
        let mut obj = serde_json::Map::new();
        obj.insert("Name".into(), JsonValue::String(ca.clone()));
        obj.insert("Email".into(), JsonValue::String(String::new()));
        obj.insert("Username".into(), JsonValue::String(String::new()));
        authors_list.push(JsonValue::Object(obj));
    }
    let authors_join = authors_list
        .iter()
        .filter_map(|a| a.get("Name").and_then(JsonValue::as_str))
        .collect::<Vec<_>>()
        .join(", ");
    vars.set("Authors", &authors_join);
    vars.set_structured("AuthorsList", JsonValue::Array(authors_list));
    // For per-entry Logins: include the primary commit login when present.
    // Co-author logins aren't extractable from the email-only trailer
    // without an extra GitHub API lookup, so the per-entry list contains
    // just the primary unless the trailer itself was a `<user@github>`
    // login form (left as future work).
    let mut commit_logins: Vec<String> = Vec::new();
    if !commit.login.is_empty() {
        commit_logins.push(commit.login.clone());
    }
    vars.set("Logins", &commit_logins.join(", "));
    // `Logins` as a structured list too — symmetric with `AuthorsList`
    // so `{{ Logins | englishJoin }}` from a dotted-variable config works
    // (`.Logins` is a list while ours has historically been a
    // comma-string for the bare `{{ Logins }}` render. The structured
    // alias under `LoginsList` lets templates iterate or filter without
    // re-splitting on commas.
    let logins_list: Vec<JsonValue> = commit_logins
        .iter()
        .map(|s| JsonValue::String(s.clone()))
        .collect();
    vars.set_structured("LoginsList", JsonValue::Array(logins_list));
    vars.set("AllLogins", logins);
    vars.set("AllAuthors", all_authors);
    let rendered = template::render(tmpl, &vars).with_context(|| {
        format!(
            "changelog: render commit format template '{tmpl}' for commit {}",
            commit.hash
        )
    })?;
    out.push_str(&format!("* {}{}", rendered, newline));
    Ok(())
}
// ---------------------------------------------------------------------------
// Public render API — used by `anodizer bump --commit` to bundle a changelog
// edit alongside the version bump in a single commit.
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

/// Load the `changelog:` block from `<workspace_root>/.anodizer.yaml`.
///
/// Returns `Ok(None)` when the file is absent or carries no `changelog:`
/// section. Deliberately skips the include/deprecation machinery in
/// `cli::pipeline::load_config` so this stays usable from non-CLI contexts
/// (core is already in the dep graph; pulling cli in would create a cycle).
fn load_changelog_config(
    workspace_root: &std::path::Path,
) -> Result<Option<anodizer_core::config::ChangelogConfig>> {
    let cfg_path = workspace_root.join(".anodizer.yaml");
    if !cfg_path.is_file() {
        return Ok(None);
    }
    let cfg_text = std::fs::read_to_string(&cfg_path)
        .with_context(|| format!("failed to read {}", cfg_path.display()))?;
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(&cfg_text)
        .with_context(|| format!("failed to parse YAML at {}", cfg_path.display()))?;
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

/// Load the crate's changelog config, fetch path-filtered commits since
/// `from_tag`, filter/sort/group them, and render the grouped commit body
/// (`### <GroupTitle>` group headings, no `## <version>` heading).
///
/// Single-sources the config-load + commit-fetch + render pipeline shared by
/// [`render_crate_section`] (per-crate file) and [`render_root_section`]
/// (shared root file) so the two entry points can never drift.
///
/// Returns the resolved [`ChangelogConfig`] alongside the body. Returns
/// `Ok(None)` under the same conditions both callers treat as "nothing to
/// release": `.anodizer.yaml` is absent / has no `changelog:` block, or there
/// are no qualifying commits since `from_tag`.
fn render_section_body(
    workspace_root: &std::path::Path,
    crate_path: &std::path::Path,
    from_tag: Option<&str>,
) -> Result<Option<(anodizer_core::config::ChangelogConfig, String)>> {
    use anodizer_core::log::{StageLogger, Verbosity};

    let Some(cfg) = load_changelog_config(workspace_root)? else {
        return Ok(None);
    };

    let log = StageLogger::new("bump-changelog", Verbosity::default());

    let path_filter = relative_filter(workspace_root, crate_path);
    let raw_commits = fetch_git_commits_in(workspace_root, from_tag, path_filter.as_deref())?;
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

    let exclude: Vec<String> = cfg
        .filters
        .as_ref()
        .and_then(|f| f.exclude.clone())
        .unwrap_or_default();
    let include: Vec<String> = cfg
        .filters
        .as_ref()
        .and_then(|f| f.include.clone())
        .unwrap_or_default();
    infos = if !include.is_empty() {
        apply_include_filters(&infos, &include, &log)?
    } else {
        apply_filters(&infos, &exclude, &log)?
    };

    sort_commits(&mut infos, cfg.resolved_sort()?)?;

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
        },
    )?;
    // `render_changelog_with_provider` always emits a `## <title>` line; we
    // suppressed it by passing `Some("")`, which produces `## \n\n`. Drop
    // any empty heading at the start so the caller's `## [<version>]` heading
    // stands alone.
    let body = body.trim_start_matches("## \n\n").trim_start().to_string();

    Ok(Some((cfg, body)))
}

/// Render a `## [<to_version>]` section for the given crate's changelog and
/// merge it into the crate's `CHANGELOG.md`.
///
/// Used by `anodizer bump --commit` to produce a single staged file edit that
/// can be bundled into the bump commit.
///
/// Returns `Ok(None)` when:
///   - `<workspace_root>/.anodizer.yaml` is absent, unreadable, or has no
///     `changelog:` section
///   - there are no qualifying commits since `from_tag` (or `HEAD` history,
///     when `from_tag` is `None`) touching `crate_path`
///
/// On success, the returned [`ChangelogUpdate`] always carries the FULL final
/// file content with [`InsertionMode::Replace`]: the function reads any
/// existing `CHANGELOG.md`, prepends the new section after the leading H1
/// header (creating one when missing), and returns the merged text.
pub fn render_crate_section(
    workspace_root: &std::path::Path,
    crate_name: &str,
    crate_path: &std::path::Path,
    from_tag: Option<&str>,
    to_version: &str,
) -> Result<Option<ChangelogUpdate>> {
    let Some((_cfg, body)) = render_section_body(workspace_root, crate_path, from_tag)? else {
        return Ok(None);
    };

    let section_heading = format!(
        "## [{ver}] - {date}",
        ver = to_version,
        date = today_yyyy_mm_dd()
    );
    let new_section = format!("{}\n\n{}\n", section_heading, body.trim_end());

    let file_path = crate_path.join("CHANGELOG.md");
    let merged = merge_into_changelog(MergeArgs {
        file_path: &file_path,
        crate_name,
        new_section: &new_section,
        generated_body: body.trim_end(),
        from_tag,
        to_version,
        workspace_root,
    })?;

    Ok(Some(ChangelogUpdate {
        file_path,
        rendered_text: merged,
        insertion_mode: InsertionMode::Replace,
    }))
}

/// Render a release section for `crate_name` and promote it into the SHARED
/// root `<workspace_root>/CHANGELOG.md` (NOT the per-crate file).
///
/// A multi-track workspace keeps one root `CHANGELOG.md` whose
/// `## [Unreleased]` holds a `### <crate>` subsection per crate. Tagging a
/// track promotes ONLY that track's subsection into a released
/// `## [<tag>] - <date>` section — re-leveled to `### <GroupTitle>` headings
/// and regrouped per the configured `groups:` — and leaves every other crate's
/// subsection in place. A single-track root (no `### <crate>` subsections)
/// falls through to the flat Keep-a-Changelog roll, byte-identical to
/// [`render_crate_section`]'s behaviour.
///
/// `tag` is the FULL new tag for this release (e.g. `v0.7.0` or
/// `core-v0.5.1`); the promoted heading and the rolled compare-link footer
/// both derive from it and from this track's own `from_tag`, so multi-track
/// compare ranges stay correct even when the shared `[Unreleased]:` anchor
/// belongs to a different track. `chronology` slots the new section among the
/// existing released sections (`Date`: newest-on-top; `Tag`: clustered by
/// tag-prefix, semver-descending within a cluster).
///
/// Returns `Ok(None)` when there is nothing to release for this track: no
/// `changelog:` config / no commits AND no curated `### <crate>` subsection.
pub fn render_root_section(
    workspace_root: &std::path::Path,
    crate_name: &str,
    crate_path: &std::path::Path,
    from_tag: Option<&str>,
    to_version: &str,
    tag: &str,
    chronology: anodizer_core::config::Chronology,
) -> Result<Option<ChangelogUpdate>> {
    let rendered = render_section_body(workspace_root, crate_path, from_tag)?;
    // Load groups from config directly (not just from `rendered`) so a curated
    // subsection with no qualifying commits still buckets under the configured
    // group headings.
    let groups = load_changelog_config(workspace_root)?
        .and_then(|c| c.groups)
        .unwrap_or_default();
    let group_titles: Vec<String> = groups.iter().map(|g| g.title.clone()).collect();
    let generated_body = rendered
        .as_ref()
        .map(|(_, body)| body.clone())
        .unwrap_or_default();

    let file_path = workspace_root.join("CHANGELOG.md");

    // Absent root file: synthesize the flat first-write exactly as the
    // per-crate path does (there is no Unreleased shape to promote into).
    if !file_path.is_file() {
        let Some((_cfg, body)) = rendered else {
            return Ok(None);
        };
        let section_heading = format!(
            "## [{ver}] - {date}",
            ver = to_version,
            date = today_yyyy_mm_dd()
        );
        let new_section = format!("{}\n\n{}\n", section_heading, body.trim_end());
        let merged = merge_into_changelog(MergeArgs {
            file_path: &file_path,
            crate_name,
            new_section: &new_section,
            generated_body: body.trim_end(),
            from_tag,
            to_version,
            workspace_root,
        })?;
        return Ok(Some(ChangelogUpdate {
            file_path,
            rendered_text: merged,
            insertion_mode: InsertionMode::Replace,
        }));
    }

    let existing = std::fs::read_to_string(&file_path)
        .with_context(|| format!("failed to read {}", file_path.display()))?;

    // Degenerate root (no `### <crate>` subsections under `[Unreleased]`, or no
    // `[Unreleased]` at all): delegate to the existing flat roll / splice so a
    // single-track root behaves exactly as `render_crate_section` would.
    if !has_crate_subsections(&existing, &group_titles) {
        let Some((_cfg, body)) = rendered else {
            return Ok(None);
        };
        let section_heading = format!(
            "## [{ver}] - {date}",
            ver = to_version,
            date = today_yyyy_mm_dd()
        );
        let new_section = format!("{}\n\n{}\n", section_heading, body.trim_end());
        let merged = merge_into_changelog(MergeArgs {
            file_path: &file_path,
            crate_name,
            new_section: &new_section,
            generated_body: body.trim_end(),
            from_tag,
            to_version,
            workspace_root,
        })?;
        return Ok(Some(ChangelogUpdate {
            file_path,
            rendered_text: merged,
            insertion_mode: InsertionMode::Replace,
        }));
    }

    // Subsection-promote path. Resolve a compare base from an existing footer
    // link, falling back to the `origin` remote, so the rolled footer keeps the
    // file's host (self-hosted GitLab/Gitea stays host-correct).
    let base = resolve_compare_base(&existing, workspace_root);

    let Some(merged) = promote_subsection(PromoteArgs {
        existing: &existing,
        crate_name,
        tag,
        from_tag,
        chronology,
        groups: &groups,
        generated_body: generated_body.trim_end(),
        base: base.as_deref(),
    })?
    else {
        return Ok(None);
    };

    Ok(Some(ChangelogUpdate {
        file_path,
        rendered_text: merged,
        insertion_mode: InsertionMode::Replace,
    }))
}

/// Resolve the `<base>/compare` URL prefix for footer links: prefer the base
/// embedded in an existing `[Unreleased]:` compare link, else synthesize one
/// from the `origin` remote. Returns `None` when neither is available (the
/// footer roll then leaves links absent rather than emitting a 404).
fn resolve_compare_base(existing: &str, workspace_root: &std::path::Path) -> Option<String> {
    if let Some(url) = existing.lines().find_map(parse_unreleased_footer)
        && let Some((base, _anchor)) = parse_compare_url(url)
    {
        return Some(base.to_string());
    }
    anodizer_core::git::detect_remote_web_base_in(workspace_root).ok()
}

/// Whether `existing` has at least one `### <crate>` subsection under its
/// `## [Unreleased]` heading (the marker of a multi-track root).
///
/// A single-track flat `[Unreleased]` may itself carry `### <GroupTitle>`
/// headings (e.g. `### Features`) for its curated body; those are NOT crate
/// subsections. `group_titles` lists the configured `groups:` titles so a
/// `### <name>` matching a group title is excluded, disambiguating
/// `### Features` (a group heading) from `### cfgd` (a crate subsection). A
/// flat `[Unreleased]` with only group headings — or no `### ` lines, or no
/// `[Unreleased]` at all — returns `false` so the caller takes the flat roll.
fn has_crate_subsections(existing: &str, group_titles: &[String]) -> bool {
    let lines: Vec<&str> = existing.lines().collect();
    let Some(unreleased_idx) = lines.iter().position(|l| is_unreleased_heading(l)) else {
        return false;
    };
    for line in lines.iter().skip(unreleased_idx + 1) {
        if is_section_heading(line) || parse_unreleased_footer(line).is_some() {
            return false;
        }
        if let Some(name) = is_subsection_heading(line)
            && !group_titles.iter().any(|t| t == name)
        {
            return true;
        }
    }
    false
}

/// Whether `line` is an H3 `### <name>` subsection heading, returning the
/// trimmed `<name>`. Matches exactly three leading hashes (so a deeper `####`
/// is not mistaken for a crate subsection).
fn is_subsection_heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_end();
    let rest = trimmed.strip_prefix("### ")?;
    if rest.starts_with('#') {
        return None;
    }
    let name = rest.trim();
    if name.is_empty() { None } else { Some(name) }
}

/// Inputs to [`promote_subsection`], the pure root-CHANGELOG transform.
struct PromoteArgs<'a> {
    /// Current root `CHANGELOG.md` contents.
    existing: &'a str,
    /// Crate whose `### <crate>` subsection is being promoted.
    crate_name: &'a str,
    /// FULL new tag for this release (e.g. `v0.7.0`, `core-v0.5.1`).
    tag: &'a str,
    /// This track's previous tag, or `None` for its first release.
    from_tag: Option<&'a str>,
    /// Section ordering for slotting the promoted section.
    chronology: anodizer_core::config::Chronology,
    /// Configured commit groups, used to bucket curated bullets.
    groups: &'a [ChangelogGroup],
    /// Generated grouped body (already `### <GroupTitle>`-grouped), used when
    /// the crate has commits but no curated subsection.
    generated_body: &'a str,
    /// `<base>/compare` URL prefix for footer links, or `None` to omit them.
    base: Option<&'a str>,
}

/// Pure transform: promote `crate_name`'s `### <crate>` subsection out of
/// `## [Unreleased]` into a released `## [<tag>] - <date>` section, regroup its
/// bullets under `### <GroupTitle>` headings, slot it by `chronology`, and roll
/// the per-track compare-link footer. Returns `Ok(None)` when the crate has
/// neither a curated subsection nor generated commits (nothing to release).
fn promote_subsection(args: PromoteArgs<'_>) -> Result<Option<String>> {
    let PromoteArgs {
        existing,
        crate_name,
        tag,
        from_tag,
        chronology,
        groups,
        generated_body,
        base,
    } = args;

    let lines: Vec<&str> = existing.lines().collect();

    // Idempotence: a `## [<tag>]` section already present means this track's
    // roll already happened — return the file unchanged.
    if lines.iter().any(|l| is_version_heading(l, tag)) {
        return Ok(Some(existing.to_string()));
    }

    let Some(unreleased_idx) = lines.iter().position(|l| is_unreleased_heading(l)) else {
        return Ok(Some(existing.to_string()));
    };

    // Bound the `[Unreleased]` block: up to the first `## ` section heading or
    // footer-link line.
    let mut unreleased_end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(unreleased_idx + 1) {
        if is_section_heading(line) || parse_unreleased_footer(line).is_some() {
            unreleased_end = i;
            break;
        }
    }

    // Locate this crate's `### <crate>` subsection within `[Unreleased]`.
    let mut sub_start: Option<usize> = None;
    let mut idx = unreleased_idx + 1;
    while idx < unreleased_end {
        if let Some(name) = is_subsection_heading(lines[idx])
            && name == crate_name
        {
            sub_start = Some(idx);
            break;
        }
        idx += 1;
    }

    // Curated bullets (verbatim) when the subsection exists; the bounds run
    // from after its heading to the next `### `/`## `/footer line.
    let curated: Vec<&str> = match sub_start {
        Some(start) => {
            let mut end = unreleased_end;
            for (i, line) in lines.iter().enumerate().skip(start + 1) {
                if is_subsection_heading(line).is_some()
                    || is_section_heading(line)
                    || parse_unreleased_footer(line).is_some()
                {
                    end = i;
                    break;
                }
            }
            lines[start + 1..end]
                .iter()
                .copied()
                .filter(|l| !l.trim().is_empty())
                .collect()
        }
        None => Vec::new(),
    };

    // Build the promoted section body. Curated bullets are bucketed verbatim;
    // an absent / empty subsection falls back to the generated grouped body.
    let body = if !curated.is_empty() {
        bucket_curated_bullets(&curated, groups)
    } else if !generated_body.is_empty() {
        generated_body.to_string()
    } else {
        // No curated subsection and no commits: nothing to release.
        return Ok(None);
    };

    let promoted_heading = format!("## [{}] - {}", tag, today_yyyy_mm_dd());
    let mut promoted: Vec<String> = Vec::new();
    promoted.push(promoted_heading);
    // The body opens with its own `### <GroupTitle>` (or a bare bullet) right
    // under the heading — no blank line between, matching the Keep-a-Changelog
    // shape of the existing released sections.
    promoted.extend(body.lines().map(|s| s.to_string()));
    promoted.push(String::new());

    // Rebuild the `[Unreleased]` block with this crate's subsection removed and
    // every other subsection byte-identical.
    let unreleased_block = rebuild_unreleased(&lines, unreleased_idx, unreleased_end, sub_start);

    // Split the remainder (existing released sections + footer) at the footer.
    let tail = &lines[unreleased_end..];
    let footer_idx = tail
        .iter()
        .position(|l| parse_unreleased_footer(l).is_some());
    let (sections, footer): (&[&str], &[&str]) = match footer_idx {
        Some(fi) => (&tail[..fi], &tail[fi..]),
        None => (tail, &[]),
    };

    // Slot the promoted section among the existing `## [<...>]` sections.
    let mut out: Vec<String> = Vec::new();
    out.extend(unreleased_block);
    let slotted = slot_sections(sections, &promoted, tag, chronology);
    out.extend(slotted);

    // Roll the footer using THIS track's `from_tag`-derived links.
    push_root_footer(&mut out, footer, tag, from_tag, base);

    let mut result = out.join("\n");
    if existing.ends_with('\n') {
        result.push('\n');
    }
    Ok(Some(result))
}

/// Rebuild the `[Unreleased]` block (heading + remaining subsections) with the
/// subsection at `sub_start` removed. Every other line — including other
/// crates' subsections — is preserved byte-identically. A single trailing blank
/// line is kept after the block.
fn rebuild_unreleased(
    lines: &[&str],
    unreleased_idx: usize,
    unreleased_end: usize,
    sub_start: Option<usize>,
) -> Vec<String> {
    let mut block: Vec<String> = Vec::new();
    // Pre-`[Unreleased]` content (H1, prelude) stays verbatim.
    block.extend(lines[..unreleased_idx].iter().map(|s| s.to_string()));
    block.push(lines[unreleased_idx].to_string());

    // Bound of the removed subsection (if present).
    let removed_end = sub_start.map(|start| {
        let mut end = unreleased_end;
        for (i, line) in lines.iter().enumerate().skip(start + 1) {
            if is_subsection_heading(line).is_some()
                || is_section_heading(line)
                || parse_unreleased_footer(line).is_some()
            {
                end = i;
                break;
            }
        }
        end
    });

    let mut i = unreleased_idx + 1;
    while i < unreleased_end {
        if let (Some(start), Some(end)) = (sub_start, removed_end)
            && i >= start
            && i < end
        {
            i = end;
            continue;
        }
        block.push(lines[i].to_string());
        i += 1;
    }

    // Normalize to exactly one trailing blank line after the block.
    while block.last().is_some_and(|l| l.trim().is_empty()) {
        block.pop();
    }
    block.push(String::new());
    block
}

/// Bucket curated bullet lines under `### <GroupTitle>` headings, matching each
/// bullet's leading conventional-commit type against `groups` (first-match-wins,
/// a group with empty/absent `regexp` is the catch-all). Bullets are kept
/// VERBATIM — never re-rendered through the commit template. A bullet matching
/// no group and no catch-all is appended at the end under no heading so curated
/// content is never silently dropped. With no groups configured, bullets are
/// emitted flat in their original order.
fn bucket_curated_bullets(curated: &[&str], groups: &[ChangelogGroup]) -> String {
    if groups.is_empty() {
        return curated.join("\n");
    }

    // Compile group regexes in config order; an empty/absent regexp marks the
    // catch-all. Mirrors `group_commits`' first-match-wins + catch-all rules.
    let compiled: Vec<(Option<regex::Regex>, &ChangelogGroup)> = groups
        .iter()
        .map(|g| {
            let re = g
                .regexp
                .as_deref()
                .filter(|p| !p.is_empty())
                .and_then(|p| regex::Regex::new(p).ok());
            (re, g)
        })
        .collect();
    let catch_all_idx = compiled.iter().position(|(re, _)| re.is_none());

    let mut buckets: Vec<Vec<&str>> = vec![Vec::new(); compiled.len()];
    let mut unmatched: Vec<&str> = Vec::new();

    'bullet: for &line in curated {
        let payload = strip_list_marker(line);
        // Re-derive the conventional-commit subject so the group regexes match
        // against the same `raw_message` shape `group_commits` sees.
        let info = parse_commit_message(payload);
        let raw = &info.raw_message;
        for (idx, (re, _)) in compiled.iter().enumerate() {
            if catch_all_idx == Some(idx) {
                break;
            }
            if let Some(re) = re
                && re.is_match(raw)
            {
                buckets[idx].push(line);
                continue 'bullet;
            }
        }
        if let Some(ci) = catch_all_idx {
            buckets[ci].push(line);
        } else {
            unmatched.push(line);
        }
    }

    // Emit non-empty groups in `order` (config order for equal/absent order).
    let mut indexed: Vec<usize> = (0..compiled.len()).collect();
    indexed.sort_by_key(|&i| compiled[i].1.order.unwrap_or(i32::MAX));

    let mut out: Vec<String> = Vec::new();
    for &i in &indexed {
        if buckets[i].is_empty() {
            continue;
        }
        out.push(format!("### {}", compiled[i].1.title));
        out.extend(buckets[i].iter().map(|s| s.to_string()));
    }
    // Curated bullets that matched no group and had no catch-all to absorb them
    // are preserved at the end under no heading rather than dropped.
    out.extend(unmatched.iter().map(|s| s.to_string()));
    out.join("\n")
}

/// Strip a leading Markdown list marker (`- ` or `* `, with optional
/// indentation) from a bullet line, returning the bare payload.
fn strip_list_marker(line: &str) -> &str {
    let t = line.trim_start();
    t.strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .unwrap_or(t)
        .trim_start()
}

/// Insert `promoted` (the new release section) among the existing released
/// `## [<...>]` sections per `chronology`, returning the full section list.
/// Existing section bodies are never re-sorted or re-emitted — this is
/// insert-only.
fn slot_sections(
    sections: &[&str],
    promoted: &[String],
    tag: &str,
    chronology: anodizer_core::config::Chronology,
) -> Vec<String> {
    use anodizer_core::config::Chronology;

    // Index where each existing `## [<...>]` section heading begins.
    let heading_idxs: Vec<usize> = sections
        .iter()
        .enumerate()
        .filter(|(_, l)| is_section_heading(l))
        .map(|(i, _)| i)
        .collect();

    let insert_at = match chronology {
        // Date: today's section is newest — insert before the first existing
        // released section.
        Chronology::Date => heading_idxs.first().copied().unwrap_or(sections.len()),
        Chronology::Tag => tag_insert_index(sections, &heading_idxs, tag),
    };

    let mut out: Vec<String> = Vec::new();
    out.extend(sections[..insert_at].iter().map(|s| s.to_string()));
    out.extend(promoted.iter().cloned());
    out.extend(sections[insert_at..].iter().map(|s| s.to_string()));
    out
}

/// Compute the insert index (into `sections`) that keeps the `Tag` ordering
/// invariant: clusters ascend lexically by tag-prefix, and within the new tag's
/// prefix cluster versions descend by semver.
fn tag_insert_index(sections: &[&str], heading_idxs: &[usize], tag: &str) -> usize {
    let new_prefix = tag_prefix(tag);
    let new_ver = anodizer_core::git::parse_semver_tag(tag).ok();

    for &hi in heading_idxs {
        let Some(existing_tag) = section_heading_tag(sections[hi]) else {
            continue;
        };
        let existing_prefix = tag_prefix(existing_tag);
        match new_prefix.cmp(&existing_prefix) {
            std::cmp::Ordering::Less => return hi,
            std::cmp::Ordering::Greater => continue,
            std::cmp::Ordering::Equal => {
                // Same cluster: insert before the first same-prefix section whose
                // semver is strictly less than the new version (semver-descending).
                let existing_ver = anodizer_core::git::parse_semver_tag(existing_tag).ok();
                match (&new_ver, &existing_ver) {
                    (Some(nv), Some(ev)) if nv > ev => return hi,
                    (Some(_), None) => return hi,
                    // Non-semver same-prefix tags fall back to lexical descending.
                    (None, _) if tag > existing_tag => return hi,
                    _ => continue,
                }
            }
        }
    }
    sections.len()
}

/// Extract the `<tag>` from a `## [<tag>] - <date>` (or `## [<tag>]`) heading.
fn section_heading_tag(line: &str) -> Option<&str> {
    let rest = line.trim_end().strip_prefix("##")?.trim_start();
    let rest = rest.strip_prefix('[')?;
    let close = rest.find(']')?;
    Some(&rest[..close])
}

/// Roll the compare-link footer for the root subsection-promote path. The new
/// `[<tag>]:` lower bound and the `[Unreleased]:` upper anchor both derive from
/// THIS track's `tag` / `from_tag` — never from a sibling track's existing
/// `[Unreleased]:` anchor. All other `[<x>]:` footer links are preserved.
fn push_root_footer(
    out: &mut Vec<String>,
    footer: &[&str],
    tag: &str,
    from_tag: Option<&str>,
    base: Option<&str>,
) {
    let Some(base) = base else {
        // No resolvable base — keep any existing footer verbatim, add nothing.
        out.extend(footer.iter().map(|s| s.to_string()));
        return;
    };

    // Ensure a blank line separates the body from the footer block.
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.push(String::new());

    // Rolled `[Unreleased]:` anchored at this release's tag.
    out.push(format!("[Unreleased]: {}/compare/{}...HEAD", base, tag));
    // New `[<tag>]:` link when this track has a previous tag; first release of a
    // track points at the release page rather than a 404 compare range.
    if let Some(from) = from_tag {
        out.push(format!("[{}]: {}/compare/{}...{}", tag, base, from, tag));
    } else {
        out.push(format!("[{}]: {}/releases/tag/{}", tag, base, tag));
    }
    // Preserve every prior `[<x>]:` link (skip the old `[Unreleased]:`).
    for &line in footer {
        if parse_unreleased_footer(line).is_some() {
            continue;
        }
        out.push(line.to_string());
    }
}

fn today_yyyy_mm_dd() -> String {
    let secs = anodizer_core::sde::resolve_now().timestamp();
    // Days since the Unix epoch, then convert to a (y,m,d) triple via the
    // Howard Hinnant date algorithm (`days_from_civil` inverse). Avoids a
    // chrono dep purely for date formatting in changelog headings.
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}
/// Inputs to [`merge_into_changelog`].
///
/// Bundles the file location plus everything the Keep-a-Changelog roll needs
/// (the previous release ref, the new version, and the generated commit body
/// used to fill an empty `## [Unreleased]` section).
pub(crate) struct MergeArgs<'a> {
    /// Absolute path of the crate's `CHANGELOG.md` (may not yet exist).
    pub(crate) file_path: &'a std::path::Path,
    /// Crate name, used only to synthesize an H1 for an absent file.
    pub(crate) crate_name: &'a str,
    /// Fully-rendered `## [<version>] - <date>\n\n<body>\n` section used by
    /// the non-KAC splice path.
    pub(crate) new_section: &'a str,
    /// The generated commit body (no heading), used to fill a KAC
    /// `## [Unreleased]` section that the user left empty.
    pub(crate) generated_body: &'a str,
    /// Previous release tag (e.g. `v0.5.0` or `crate-v0.5.0`), or `None` for
    /// a first release. Used to derive the tag prefix when no footer link
    /// exists.
    pub(crate) from_tag: Option<&'a str>,
    /// Version being released (e.g. `0.6.0`).
    pub(crate) to_version: &'a str,
    /// Repository root, used to resolve the `origin` remote when a KAC file
    /// has a `## [Unreleased]` heading but no `[Unreleased]:` footer link.
    pub(crate) workspace_root: &'a std::path::Path,
}

/// Case-insensitive match for a `## [Unreleased]` heading line (allowing
/// trailing whitespace), the marker for a Keep-a-Changelog-shaped file.
fn is_unreleased_heading(line: &str) -> bool {
    let trimmed = line.trim_end();
    let Some(rest) = trimmed.strip_prefix("##") else {
        return false;
    };
    let rest = rest.trim_start();
    rest.eq_ignore_ascii_case("[unreleased]")
}

/// Whether a line opens a new top-level changelog section (`## ...`).
fn is_section_heading(line: &str) -> bool {
    line.starts_with("## ")
}

/// Whether `line` is a `## [<version>]` heading for the exact `version`
/// (allowing an optional ` - <date>` suffix and trailing whitespace). Used to
/// detect a same-version section already present so a second roll is a no-op.
fn is_version_heading(line: &str, version: &str) -> bool {
    let Some(rest) = line.trim_end().strip_prefix("##") else {
        return false;
    };
    let rest = rest.trim_start();
    let Some(rest) = rest.strip_prefix('[') else {
        return false;
    };
    let Some(close) = rest.find(']') else {
        return false;
    };
    &rest[..close] == version
}

/// Merge a freshly-rendered release into the crate's `CHANGELOG.md`.
///
/// Detects the Keep-a-Changelog shape (a `## [Unreleased]` heading) and, in
/// that mode, performs the standard release roll; otherwise falls back to
/// splicing `new_section` directly after the leading H1.
pub(crate) fn merge_into_changelog(args: MergeArgs<'_>) -> Result<String> {
    let MergeArgs {
        file_path,
        crate_name,
        new_section,
        generated_body,
        from_tag,
        to_version,
        workspace_root,
    } = args;

    let header = format!("# Changelog — {}\n\n", crate_name);
    if !file_path.is_file() {
        return Ok(format!("{}{}", header, new_section));
    }
    let existing = std::fs::read_to_string(file_path)
        .with_context(|| format!("failed to read {}", file_path.display()))?;

    if existing.lines().any(is_unreleased_heading) {
        return roll_keep_a_changelog(KacRollArgs {
            existing: &existing,
            generated_body,
            from_tag,
            to_version,
            workspace_root,
        });
    }

    splice_after_h1(&existing, new_section, &header)
}

/// Splice `new_section` after the leading H1, preserving any prelude.
/// Synthesizes `header` + section when the file has no H1.
fn splice_after_h1(existing: &str, new_section: &str, header: &str) -> Result<String> {
    let mut head = String::new();
    let mut tail = String::new();
    let mut consumed_h1 = false;
    let mut blank_after_h1_seen = false;
    for line in existing.lines() {
        if !consumed_h1 {
            head.push_str(line);
            head.push('\n');
            if line.starts_with("# ") {
                consumed_h1 = true;
            }
            continue;
        }
        if !blank_after_h1_seen {
            // Consume one blank line right after the H1 to keep formatting.
            if line.trim().is_empty() {
                head.push('\n');
                blank_after_h1_seen = true;
                continue;
            }
            blank_after_h1_seen = true;
        }
        tail.push_str(line);
        tail.push('\n');
    }
    if !consumed_h1 {
        // No H1 found — synthesize one and place existing content after our
        // new section.
        return Ok(format!("{}{}\n{}", header, new_section, existing));
    }
    Ok(format!("{}{}\n{}", head, new_section, tail))
}

/// Inputs to [`roll_keep_a_changelog`].
struct KacRollArgs<'a> {
    existing: &'a str,
    generated_body: &'a str,
    from_tag: Option<&'a str>,
    to_version: &'a str,
    workspace_root: &'a std::path::Path,
}

/// Non-digit prefix of a tag/anchor (`v0.5.0` → `v`, `anodizer-v0.5.0`
/// → `anodizer-v`). Stops at the first ASCII digit.
fn tag_prefix(anchor: &str) -> String {
    anchor.chars().take_while(|c| !c.is_ascii_digit()).collect()
}

/// Perform the Keep-a-Changelog release roll on `existing`:
///   1. promote `## [Unreleased]` to `## [<version>] - <date>`,
///   2. preserve a curated body verbatim (else fill from generated commits),
///   3. insert a fresh empty `## [Unreleased]` above it,
///   4. roll the `[Unreleased]` / `[<version>]` compare-link footer.
fn roll_keep_a_changelog(args: KacRollArgs<'_>) -> Result<String> {
    let KacRollArgs {
        existing,
        generated_body,
        from_tag,
        to_version,
        workspace_root,
    } = args;

    let lines: Vec<&str> = existing.lines().collect();

    // Idempotence: if a `## [<to_version>]` section already exists, the roll
    // has already happened for this version. Promoting again would emit a
    // duplicate same-version section, so return the file unchanged.
    if lines.iter().any(|l| is_version_heading(l, to_version)) {
        return Ok(existing.to_string());
    }

    // Locate the `## [Unreleased]` heading and the start of the next section
    // (next `## ` heading) which bounds the Unreleased body. The footer link
    // block (if any) lives at or after the last section and is handled
    // separately, so the body scan also stops at the first footer-link line.
    let Some(unreleased_idx) = lines.iter().position(|l| is_unreleased_heading(l)) else {
        // Caller only invokes this when an Unreleased heading exists.
        return Ok(existing.to_string());
    };

    let mut body_end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(unreleased_idx + 1) {
        if is_section_heading(line) || parse_unreleased_footer(line).is_some() {
            body_end = i;
            break;
        }
    }

    let curated_body: Vec<&str> = lines[unreleased_idx + 1..body_end].to_vec();
    // First/last non-blank line bounds of the curated block. `Some` exactly
    // when the user left curated content under `## [Unreleased]`; `None` means
    // the section was empty and the body is filled from generated commits.
    let curated_bounds = curated_body
        .iter()
        .position(|l| !l.trim().is_empty())
        .map(|start| {
            // `rposition` is `Some` whenever `position` was, so the closing
            // bound is taken on the same guaranteed-non-empty slice.
            let end = curated_body
                .iter()
                .rposition(|l| !l.trim().is_empty())
                .map_or(start + 1, |i| i + 1);
            (start, end)
        });

    let date = today_yyyy_mm_dd();
    let promoted_heading = format!("## [{}] - {}", to_version, date);

    let mut out_lines: Vec<String> = Vec::new();
    // Everything before the Unreleased heading stays byte-identical.
    out_lines.extend(lines[..unreleased_idx].iter().map(|s| s.to_string()));

    // Fresh empty Unreleased section above the promoted release.
    out_lines.push("## [Unreleased]".to_string());
    out_lines.push(String::new());

    // Promoted release heading + its body (curated verbatim, else generated).
    out_lines.push(promoted_heading);
    out_lines.push(String::new());
    if let Some((start, end)) = curated_bounds {
        // Curated block with leading/trailing blanks trimmed, interior verbatim.
        out_lines.extend(curated_body[start..end].iter().map(|s| s.to_string()));
    } else {
        out_lines.extend(generated_body.lines().map(|s| s.to_string()));
    }
    out_lines.push(String::new());

    // Everything from the next section onward, with the footer rolled.
    let tail = &lines[body_end..];
    roll_footer(&mut out_lines, tail, from_tag, to_version, workspace_root)?;

    let mut result = out_lines.join("\n");
    if existing.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

/// Parse `[Unreleased]: <url>` (case-insensitive on the label, allowing
/// trailing whitespace) and return the URL.
fn parse_unreleased_footer(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix('[')?;
    let close = rest.find(']')?;
    let (label, after) = rest.split_at(close);
    if !label.eq_ignore_ascii_case("unreleased") {
        return None;
    }
    let after = after.strip_prefix("]:")?;
    Some(after.trim())
}

/// Split a `<base>/compare/<anchor>...HEAD` compare URL into
/// `(base_including_compare, old_anchor)`.
fn parse_compare_url(url: &str) -> Option<(&str, &str)> {
    let (base, rest) = url.split_once("/compare/")?;
    let anchor = rest.strip_suffix("...HEAD")?;
    if anchor.is_empty() {
        return None;
    }
    Some((base, anchor))
}

/// Append the tail (next section onward) to `out_lines`, rolling the
/// `[Unreleased]:` compare-link footer when one is present.
fn roll_footer(
    out_lines: &mut Vec<String>,
    tail: &[&str],
    from_tag: Option<&str>,
    to_version: &str,
    workspace_root: &std::path::Path,
) -> Result<()> {
    // Locate an existing `[Unreleased]:` footer link in the tail.
    let footer_idx = tail
        .iter()
        .position(|l| parse_unreleased_footer(l).is_some());

    let Some(footer_idx) = footer_idx else {
        // No footer link. Synthesize one only if we can resolve a remote
        // compare base cheaply; otherwise pass the tail through unchanged.
        out_lines.extend(tail.iter().map(|s| s.to_string()));
        synthesize_footer(out_lines, from_tag, to_version, workspace_root);
        return Ok(());
    };

    let url = parse_unreleased_footer(tail[footer_idx]).unwrap_or("");
    let Some((base, old_anchor)) = parse_compare_url(url) else {
        // Footer present but not a recognized compare URL — leave as-is.
        out_lines.extend(tail.iter().map(|s| s.to_string()));
        return Ok(());
    };

    let prefix = tag_prefix(old_anchor);
    let new_tag = format!("{}{}", prefix, to_version);

    // Emit tail lines up to (not including) the footer link unchanged.
    out_lines.extend(tail[..footer_idx].iter().map(|s| s.to_string()));
    // Rolled `[Unreleased]:` link + the new `[<version>]:` link.
    push_compare_footer(out_lines, base, old_anchor, &new_tag, to_version);
    // Remaining footer lines (prior `[x.y.z]:` links) unchanged.
    out_lines.extend(tail[footer_idx + 1..].iter().map(|s| s.to_string()));

    Ok(())
}

/// Push the two compare-link footer lines that close a Keep-a-Changelog roll:
///
/// ```text
/// [Unreleased]: <base>/compare/<new_tag>...HEAD
/// [<to_version>]: <base>/compare/<old_anchor>...<new_tag>
/// ```
///
/// Single-sources the compare-URL shape so the roll path and the
/// synthesize path can never drift into producing a different link layout.
fn push_compare_footer(
    out_lines: &mut Vec<String>,
    base: &str,
    old_anchor: &str,
    new_tag: &str,
    to_version: &str,
) {
    out_lines.push(format!("[Unreleased]: {}/compare/{}...HEAD", base, new_tag));
    out_lines.push(format!(
        "[{}]: {}/compare/{}...{}",
        to_version, base, old_anchor, new_tag
    ));
}

/// Synthesize a `[Unreleased]:` / `[<version>]:` footer from the `origin`
/// remote when the KAC file lacks one.
///
/// The compare base is derived from the actual `origin` URL host, so a
/// self-hosted GitLab/Gitea KAC file gets a host-correct link rather than a
/// hardcoded `github.com` one (mirroring how the roll path preserves whatever
/// base an existing footer used). Skips gracefully (no footer appended) when
/// the previous tag or the remote cannot be resolved — a missing remote must
/// never fail the render.
fn synthesize_footer(
    out_lines: &mut Vec<String>,
    from_tag: Option<&str>,
    to_version: &str,
    workspace_root: &std::path::Path,
) {
    let Some(old_anchor) = from_tag else {
        return;
    };
    let Ok(base) = anodizer_core::git::detect_remote_web_base_in(workspace_root) else {
        return;
    };
    let prefix = tag_prefix(old_anchor);
    let new_tag = format!("{}{}", prefix, to_version);

    // Ensure a blank line separates the body from a freshly-added footer block.
    if out_lines.last().is_some_and(|l| !l.is_empty()) {
        out_lines.push(String::new());
    }
    push_compare_footer(out_lines, &base, old_anchor, &new_tag, to_version);
}

#[cfg(test)]
mod root_section_tests {
    use super::*;
    use anodizer_core::config::{ChangelogGroup, Chronology};

    /// Features (`^feat`) / Bug Fixes (`^fix`) groups, mirroring a typical
    /// `groups:` config for the curated-bucketing tests.
    fn feat_fix_groups() -> Vec<ChangelogGroup> {
        vec![
            ChangelogGroup {
                title: "Features".to_string(),
                regexp: Some("^feat".to_string()),
                order: Some(0),
                groups: None,
            },
            ChangelogGroup {
                title: "Bug Fixes".to_string(),
                regexp: Some("^fix".to_string()),
                order: Some(1),
                groups: None,
            },
        ]
    }

    /// Drive the pure subsection-promote transform with a fixed compare base.
    fn promote(
        existing: &str,
        crate_name: &str,
        tag: &str,
        from_tag: Option<&str>,
        chronology: Chronology,
        groups: &[ChangelogGroup],
        generated_body: &str,
    ) -> Option<String> {
        promote_subsection(PromoteArgs {
            existing,
            crate_name,
            tag,
            from_tag,
            chronology,
            groups,
            generated_body,
            base: Some("https://github.com/tj-smith47/cfgd"),
        })
        .expect("promote_subsection succeeds")
    }

    const TWO_TRACK_FIXTURE: &str = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: add `cfgd man`\n\
- fix: env scope\n\
\n\
### cfgd-core\n\
- feat: broaden spec.env\n\
\n\
## [v0.6.0] - 2026-05-28\n\
### Features\n\
- prior cfgd thing\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n\
[v0.6.0]: https://github.com/tj-smith47/cfgd/compare/v0.5.0...v0.6.0\n";

    #[test]
    fn single_subsection_promote_curated_regrouped_date() {
        let groups = feat_fix_groups();
        let out = promote(
            TWO_TRACK_FIXTURE,
            "cfgd",
            "v0.7.0",
            Some("v0.6.0"),
            Chronology::Date,
            &groups,
            "",
        )
        .expect("some output");

        let date = today_yyyy_mm_dd();
        let expected = format!(
            "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd-core\n\
- feat: broaden spec.env\n\
\n\
## [v0.7.0] - {date}\n\
### Features\n\
- feat: add `cfgd man`\n\
### Bug Fixes\n\
- fix: env scope\n\
\n\
## [v0.6.0] - 2026-05-28\n\
### Features\n\
- prior cfgd thing\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.7.0...HEAD\n\
[v0.7.0]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...v0.7.0\n\
[v0.6.0]: https://github.com/tj-smith47/cfgd/compare/v0.5.0...v0.6.0\n"
        );
        assert_eq!(out, expected, "exact root promote output");
    }

    #[test]
    fn other_subsections_retained_byte_identical() {
        let groups = feat_fix_groups();
        let out = promote(
            TWO_TRACK_FIXTURE,
            "cfgd",
            "v0.7.0",
            Some("v0.6.0"),
            Chronology::Date,
            &groups,
            "",
        )
        .expect("some output");

        // The non-promoted crate's subsection survives verbatim.
        assert!(
            out.contains("### cfgd-core\n- feat: broaden spec.env"),
            "cfgd-core subsection must be retained verbatim: {out}"
        );
        // The promoted crate's subsection is gone from Unreleased.
        let unreleased = out
            .split("## [v0.7.0]")
            .next()
            .expect("text before promoted section");
        assert!(
            !unreleased.contains("### cfgd\n"),
            "promoted ### cfgd subsection must be removed from Unreleased: {unreleased}"
        );
    }

    #[test]
    fn date_slots_new_section_directly_under_unreleased() {
        let groups = feat_fix_groups();
        let out = promote(
            TWO_TRACK_FIXTURE,
            "cfgd",
            "v0.7.0",
            Some("v0.6.0"),
            Chronology::Date,
            &groups,
            "",
        )
        .expect("some output");

        let promoted = out.find("## [v0.7.0]").expect("promoted heading");
        let prior = out.find("## [v0.6.0]").expect("prior heading");
        assert!(
            promoted < prior,
            "date chronology puts today's section above older releases: {out}"
        );
    }

    /// Five-release reference timeline shared by the Tag/Date ordering tests.
    /// Two `### crate` subsections under Unreleased so a promote keeps the
    /// multi-track shape, plus four prior released sections in mixed order.
    fn five_release_fixture() -> String {
        "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: new cfgd\n\
\n\
### cfgd-core\n\
- feat: new core\n\
\n\
## [core-v0.5.0] - 2026-05-20\n\
### Features\n\
- core 0.5.0\n\
\n\
## [v0.6.0] - 2026-05-10\n\
### Features\n\
- cfgd 0.6.0\n\
\n\
## [core-v0.4.0] - 2026-05-01\n\
### Features\n\
- core 0.4.0\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/core-v0.5.0...HEAD\n\
[core-v0.5.0]: https://github.com/tj-smith47/cfgd/compare/core-v0.4.0...core-v0.5.0\n\
[v0.6.0]: https://github.com/tj-smith47/cfgd/compare/v0.5.0...v0.6.0\n\
[core-v0.4.0]: https://github.com/tj-smith47/cfgd/releases/tag/core-v0.4.0\n"
            .to_string()
    }

    /// Same release set as [`five_release_fixture`], but with the existing
    /// released sections already in valid `Tag` order (prefix-clustered,
    /// semver-descending): `core-v0.5.0, core-v0.4.0, v0.6.0`. A `Tag`-mode
    /// promote must keep that invariant after inserting the new section.
    fn five_release_tag_ordered_fixture() -> String {
        "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: new cfgd\n\
\n\
### cfgd-core\n\
- feat: new core\n\
\n\
## [core-v0.5.0] - 2026-05-20\n\
### Features\n\
- core 0.5.0\n\
\n\
## [core-v0.4.0] - 2026-05-01\n\
### Features\n\
- core 0.4.0\n\
\n\
## [v0.6.0] - 2026-05-10\n\
### Features\n\
- cfgd 0.6.0\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/core-v0.5.0...HEAD\n\
[core-v0.5.0]: https://github.com/tj-smith47/cfgd/compare/core-v0.4.0...core-v0.5.0\n\
[core-v0.4.0]: https://github.com/tj-smith47/cfgd/releases/tag/core-v0.4.0\n\
[v0.6.0]: https://github.com/tj-smith47/cfgd/compare/v0.5.0...v0.6.0\n"
            .to_string()
    }

    /// Collect the ordered list of `## [<tag>]` section tags from a rendered
    /// changelog (excluding `[Unreleased]`).
    fn section_order(text: &str) -> Vec<String> {
        text.lines()
            .filter_map(section_heading_tag)
            .filter(|t| !t.eq_ignore_ascii_case("unreleased"))
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn tag_clusters_by_prefix_then_semver_desc() {
        let groups = feat_fix_groups();
        // Tag v0.7.0 into a Tag-ordered timeline (core-v0.5.0, core-v0.4.0,
        // v0.6.0). Tag ordering clusters `core-` (asc lexical) before `v`,
        // semver-desc within each cluster, so v0.7.0 lands at the head of the
        // `v` cluster (before v0.6.0) and after the whole `core-` cluster.
        let out = promote(
            &five_release_tag_ordered_fixture(),
            "cfgd",
            "v0.7.0",
            Some("v0.6.0"),
            Chronology::Tag,
            &groups,
            "",
        )
        .expect("some output");

        assert_eq!(
            section_order(&out),
            vec!["core-v0.5.0", "core-v0.4.0", "v0.7.0", "v0.6.0"],
            "tag chronology clusters by prefix then semver-desc: {out}"
        );
    }

    #[test]
    fn date_orders_newest_first_distinct_from_tag() {
        let groups = feat_fix_groups();
        let out = promote(
            &five_release_fixture(),
            "cfgd",
            "v0.7.0",
            Some("v0.6.0"),
            Chronology::Date,
            &groups,
            "",
        )
        .expect("some output");

        // Date inserts today's section at the very top of the version list,
        // leaving all existing sections in their file order.
        assert_eq!(
            section_order(&out),
            vec!["v0.7.0", "core-v0.5.0", "v0.6.0", "core-v0.4.0"],
            "date chronology keeps today on top, others unchanged: {out}"
        );
    }

    #[test]
    fn multitrack_footer_derives_tag_from_own_from_tag_not_unreleased_anchor() {
        // The shared `[Unreleased]:` anchor belongs to the `core-` track
        // (core-v0.5.0), but we tag the `v` track. The new tag and compare
        // lower-bound MUST come from this track's from_tag (v0.6.0), not the
        // anchor.
        let groups = feat_fix_groups();
        let out = promote(
            &five_release_fixture(),
            "cfgd",
            "v0.7.0",
            Some("v0.6.0"),
            Chronology::Date,
            &groups,
            "",
        )
        .expect("some output");

        assert!(
            out.contains("[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.7.0...HEAD"),
            "Unreleased anchor must roll to this track's new tag: {out}"
        );
        assert!(
            out.contains("[v0.7.0]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...v0.7.0"),
            "new compare link must use this track's from_tag, not the anchor: {out}"
        );
        assert!(
            !out.contains("core-v0.7.0"),
            "must NOT synthesize a core-prefixed tag from the shared anchor: {out}"
        );
        // Pre-existing footer links survive.
        assert!(
            out.contains("[v0.6.0]: https://github.com/tj-smith47/cfgd/compare/v0.5.0...v0.6.0"),
            "prior footer links preserved: {out}"
        );
    }

    #[test]
    fn generated_fill_when_subsection_absent() {
        // A root with `### other` subsections under Unreleased but NO `### cfgd`
        // subsection: cfgd still gets a section from the generated body.
        let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd-core\n\
- feat: core work\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n";
        let groups = feat_fix_groups();
        let generated = "### Features\n- feat: generated cfgd commit";
        let out = promote(
            existing,
            "cfgd",
            "v0.7.0",
            Some("v0.6.0"),
            Chronology::Date,
            &groups,
            generated,
        )
        .expect("some output");

        assert!(
            out.contains("## [v0.7.0] - "),
            "generated section heading present: {out}"
        );
        assert!(
            out.contains("- feat: generated cfgd commit"),
            "generated body fills the section: {out}"
        );
        // The unrelated subsection is untouched.
        assert!(
            out.contains("### cfgd-core\n- feat: core work"),
            "other subsection retained: {out}"
        );
    }

    #[test]
    fn returns_none_when_no_subsection_and_no_commits() {
        let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd-core\n\
- feat: core work\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n";
        let groups = feat_fix_groups();
        let out = promote(
            existing,
            "cfgd",
            "v0.7.0",
            Some("v0.6.0"),
            Chronology::Date,
            &groups,
            "",
        );
        assert!(
            out.is_none(),
            "no curated subsection and no generated commits → nothing to release"
        );
    }

    #[test]
    fn idempotent_second_promote_is_noop() {
        let groups = feat_fix_groups();
        let first = promote(
            TWO_TRACK_FIXTURE,
            "cfgd",
            "v0.7.0",
            Some("v0.6.0"),
            Chronology::Date,
            &groups,
            "",
        )
        .expect("first promote");

        // Re-running with the same tag must be a no-op: the `## [v0.7.0]`
        // section already exists.
        let second = promote(
            &first,
            "cfgd",
            "v0.7.0",
            Some("v0.6.0"),
            Chronology::Date,
            &groups,
            "",
        )
        .expect("second promote");
        assert_eq!(first, second, "second promote with same tag is a no-op");
    }

    #[test]
    fn curated_bullet_with_no_group_and_no_catchall_is_preserved() {
        let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: a feature\n\
- docs: update readme\n\
\n\
### cfgd-core\n\
- feat: core\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n";
        // Only Features (^feat) configured; no catch-all. The `docs:` bullet
        // matches no group and must NOT be dropped.
        let groups = vec![ChangelogGroup {
            title: "Features".to_string(),
            regexp: Some("^feat".to_string()),
            order: Some(0),
            groups: None,
        }];
        let out = promote(
            existing,
            "cfgd",
            "v0.7.0",
            Some("v0.6.0"),
            Chronology::Date,
            &groups,
            "",
        )
        .expect("some output");

        assert!(
            out.contains("### Features\n- feat: a feature"),
            "feat bullet bucketed under Features: {out}"
        );
        assert!(
            out.contains("- docs: update readme"),
            "unmatched curated bullet must be preserved, not dropped: {out}"
        );
    }

    #[test]
    fn no_groups_emits_curated_bullets_flat() {
        let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: a feature\n\
- fix: a fix\n\
\n\
### cfgd-core\n\
- feat: core\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n";
        let out = promote(
            existing,
            "cfgd",
            "v0.7.0",
            Some("v0.6.0"),
            Chronology::Date,
            &[],
            "",
        )
        .expect("some output");

        let date = today_yyyy_mm_dd();
        // No group headings — bullets stay flat under the version heading.
        assert!(
            out.contains(&format!(
                "## [v0.7.0] - {date}\n- feat: a feature\n- fix: a fix\n"
            )),
            "no groups → flat bullets, no ### headings: {out}"
        );
    }

    #[test]
    fn degenerate_flat_root_uses_bare_version_heading() {
        // A flat `[Unreleased]` with NO `### crate` subsections takes the flat
        // KaC roll path (bare `## [<version>]` heading, not the full tag).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("CHANGELOG.md");
        // A genuinely flat single-track `[Unreleased]`: bare bullets, no `###`
        // group/crate subsections. This is the degenerate N=1 shape.
        std::fs::write(
            &path,
            "# Changelog\n\
\n\
## [Unreleased]\n\
- a feature\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n",
        )
        .expect("write fixture");

        // No `### crate` subsection → has_crate_subsections == false.
        let existing = std::fs::read_to_string(&path).expect("read");
        assert!(
            !has_crate_subsections(&existing, &[]),
            "flat Unreleased has no crate subsections"
        );

        // The flat path emits a bare-version heading. Drive it through the
        // same merge the degenerate branch of render_root_section uses.
        let date = today_yyyy_mm_dd();
        let new_section = format!("## [0.7.0] - {date}\n\n- a feature\n");
        let merged = merge_into_changelog(MergeArgs {
            file_path: &path,
            crate_name: "cfgd",
            new_section: &new_section,
            generated_body: "- a feature",
            from_tag: Some("v0.6.0"),
            to_version: "0.7.0",
            workspace_root: dir.path(),
        })
        .expect("flat merge");

        assert!(
            merged.contains(&format!("## [0.7.0] - {date}")),
            "degenerate flat root uses a bare-version heading: {merged}"
        );
        assert!(
            !merged.contains("## [v0.7.0]"),
            "flat path must NOT use the full tag in the heading: {merged}"
        );
    }

    #[test]
    fn group_headings_under_flat_unreleased_are_not_crate_subsections() {
        // A single-track flat `[Unreleased]` whose curated body uses
        // `### Features` / `### Bug Fixes` group headings must NOT be mistaken
        // for a multi-track root, given those titles are configured groups.
        let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
### Features\n\
- a feature\n\
### Bug Fixes\n\
- a fix\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n";
        let titles = vec!["Features".to_string(), "Bug Fixes".to_string()];
        assert!(
            !has_crate_subsections(existing, &titles),
            "group headings are not crate subsections when titles are configured"
        );
        // Without the group titles, the same `### Features` heading would be
        // (mis)read as a crate subsection — proving the disambiguation is what
        // separates the two shapes.
        assert!(
            has_crate_subsections(existing, &[]),
            "absent group titles, any ### heading reads as a crate subsection"
        );
    }

    #[test]
    fn first_release_footer_points_at_release_tag_not_compare() {
        // First release of a track: `from_tag=None`. The new `[<tag>]:` link
        // must point at the release page (no 404 compare range), while the
        // rolled `[Unreleased]:` anchor still advances to this release's tag.
        let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: first ever\n\
\n\
### cfgd-core\n\
- feat: core work\n";
        let groups = feat_fix_groups();
        let out = promote(
            existing,
            "cfgd",
            "v0.7.0",
            None,
            Chronology::Date,
            &groups,
            "",
        )
        .expect("some output");

        assert!(
            out.contains("[v0.7.0]: https://github.com/tj-smith47/cfgd/releases/tag/v0.7.0"),
            "first release must link the release page, not a compare range: {out}"
        );
        assert!(
            !out.contains("compare/...v0.7.0") && !out.contains("/compare/None"),
            "first release must NOT synthesize a compare lower-bound: {out}"
        );
        assert!(
            out.contains("[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.7.0...HEAD"),
            "Unreleased anchor must still roll to this release's tag: {out}"
        );
    }

    /// Initialize a fresh git repo with `user`/`email` configured and a single
    /// `feat:` commit, so the commit-driven `render_root_section` branch has
    /// real history. Mirrors the repo setup the existing stage tests use.
    fn init_repo_with_commit(dir: &std::path::Path) {
        use std::process::Command;
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            let ok = Command::new("git")
                .args(&args)
                .current_dir(dir)
                .status()
                .expect("git command runs")
                .success();
            assert!(ok, "git {args:?} failed");
        }
        std::fs::write(dir.join("README.md"), "seed").expect("write seed file");
        for args in [
            vec!["add", "."],
            vec!["commit", "-q", "-m", "feat: initial work"],
        ] {
            let ok = Command::new("git")
                .args(&args)
                .current_dir(dir)
                .status()
                .expect("git command runs")
                .success();
            assert!(ok, "git {args:?} failed");
        }
    }

    /// Tag the current HEAD with `tag`, then add `file_change` as a fresh
    /// commit, so `<tag>..HEAD` resolves to a non-empty range.
    fn tag_and_commit(dir: &std::path::Path, tag: &str, message: &str) {
        use std::process::Command;
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .expect("git command runs")
                .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["tag", tag]);
        std::fs::write(dir.join("post.txt"), "post-tag").expect("write post-tag file");
        run(&["add", "."]);
        run(&["commit", "-q", "-m", message]);
    }

    /// Write a minimal `.anodizer.yaml` carrying a `changelog:` block with
    /// Features/Bug Fixes groups so config-load + grouping resolve.
    fn write_anodizer_yaml(dir: &std::path::Path) {
        // A raw string keeps the YAML block's leading indentation intact (a
        // `\`-continued string literal would strip it and break the parse).
        let yaml = r#"changelog:
  groups:
    - title: Features
      regexp: '^feat'
      order: 0
    - title: Bug Fixes
      regexp: '^fix'
      order: 1
"#;
        std::fs::write(dir.join(".anodizer.yaml"), yaml).expect("write .anodizer.yaml");
    }

    #[test]
    fn render_root_section_absent_file_creates_initial_root() {
        // IO branch (a): no root CHANGELOG.md yet, but the crate has commits →
        // synthesize the initial root file with a bare `## [<to_version>]`
        // first-write section.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        init_repo_with_commit(root);
        write_anodizer_yaml(root);

        let update = render_root_section(
            root,
            "cfgd",
            root,
            None,
            "0.7.0",
            "v0.7.0",
            Chronology::Date,
        )
        .expect("render_root_section succeeds")
        .expect("commits present → an update is produced");

        assert_eq!(
            update.file_path,
            root.join("CHANGELOG.md"),
            "writes the root file, not a per-crate file"
        );
        let text = &update.rendered_text;
        let date = today_yyyy_mm_dd();
        assert!(
            text.contains(&format!("## [0.7.0] - {date}")),
            "initial root carries a bare-version first-write heading: {text}"
        );
        // The `feat: initial work` commit is grouped under Features; the
        // default git format renders it as `<sha> initial work` (the
        // conventional-commit prefix is consumed by the group match).
        assert!(
            text.contains("### Features"),
            "the commit is grouped under its configured heading: {text}"
        );
        assert!(
            text.contains("initial work"),
            "the commit feeds the generated body: {text}"
        );
    }

    #[test]
    fn render_root_section_degenerate_flat_uses_bare_version_heading() {
        // IO branch (b): a flat `[Unreleased]` with NO `### crate` subsections
        // (only group headings) → flat roll, bare `## [<to_version>]` heading.
        // The flat roll is commit-gated (same `render_section_body` gate as
        // `render_crate_section`), so a real commit must be present for it to
        // fire; the curated `[Unreleased]` body is then promoted verbatim.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // Commit, tag v0.6.0, then a NEW commit so `v0.6.0..HEAD` is non-empty
        // and the commit-gated flat roll fires.
        init_repo_with_commit(root);
        tag_and_commit(root, "v0.6.0", "feat: post-tag work");
        write_anodizer_yaml(root);
        let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### Features\n\
- a curated feature\n\
\n\
## [v0.6.0] - 2026-05-28\n\
### Features\n\
- prior thing\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n\
[v0.6.0]: https://github.com/tj-smith47/cfgd/compare/v0.5.0...v0.6.0\n";
        std::fs::write(root.join("CHANGELOG.md"), existing).expect("write root");

        let update = render_root_section(
            root,
            "cfgd",
            root,
            Some("v0.6.0"),
            "0.7.0",
            "v0.7.0",
            Chronology::Date,
        )
        .expect("render_root_section succeeds")
        .expect("curated flat Unreleased → an update is produced");

        let text = &update.rendered_text;
        let date = today_yyyy_mm_dd();
        assert!(
            text.contains(&format!("## [0.7.0] - {date}")),
            "degenerate flat root uses the bare-version heading: {text}"
        );
        assert!(
            !text.contains("## [v0.7.0]"),
            "flat path must NOT use the full tag in the heading: {text}"
        );
        // Curated content is promoted verbatim by the flat roll.
        assert!(
            text.contains("- a curated feature"),
            "curated body promoted verbatim: {text}"
        );
    }

    #[test]
    fn render_root_section_subsection_promote_uses_full_tag_heading() {
        // IO branch (c): a real `### cfgd` subsection under `[Unreleased]` →
        // full subsection promote, `## [<tag>]` heading, footer base parsed
        // from the existing compare link (resolve_compare_base, not remote).
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write_anodizer_yaml(root);
        // The locked two-track fixture shape: cfgd + cfgd-core subsections.
        let existing = "# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### cfgd\n\
- feat: add `cfgd man`\n\
- fix: env scope\n\
\n\
### cfgd-core\n\
- feat: broaden spec.env\n\
\n\
## [v0.6.0] - 2026-05-28\n\
### Features\n\
- prior cfgd thing\n\
\n\
[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...HEAD\n\
[v0.6.0]: https://github.com/tj-smith47/cfgd/compare/v0.5.0...v0.6.0\n";
        std::fs::write(root.join("CHANGELOG.md"), existing).expect("write root");

        let update = render_root_section(
            root,
            "cfgd",
            root,
            Some("v0.6.0"),
            "0.7.0",
            "v0.7.0",
            Chronology::Date,
        )
        .expect("render_root_section succeeds")
        .expect("curated ### cfgd subsection → an update is produced");

        let text = &update.rendered_text;
        let date = today_yyyy_mm_dd();
        assert_eq!(
            update.file_path,
            root.join("CHANGELOG.md"),
            "promotes into the root file"
        );
        assert!(
            text.contains(&format!("## [v0.7.0] - {date}")),
            "subsection promote uses the FULL tag heading: {text}"
        );
        // Curated bullets bucketed under Features/Bug Fixes, verbatim.
        assert!(
            text.contains("### Features\n- feat: add `cfgd man`"),
            "feat bullet bucketed under Features: {text}"
        );
        assert!(
            text.contains("### Bug Fixes\n- fix: env scope"),
            "fix bullet bucketed under Bug Fixes: {text}"
        );
        // The other crate's subsection is retained verbatim under Unreleased.
        assert!(
            text.contains("### cfgd-core\n- feat: broaden spec.env"),
            "sibling subsection retained: {text}"
        );
        // resolve_compare_base parsed the existing footer's host (no remote).
        assert!(
            text.contains("[Unreleased]: https://github.com/tj-smith47/cfgd/compare/v0.7.0...HEAD"),
            "footer base parsed from the existing compare link: {text}"
        );
        assert!(
            text.contains("[v0.7.0]: https://github.com/tj-smith47/cfgd/compare/v0.6.0...v0.7.0"),
            "new compare link derives from this track's from_tag: {text}"
        );
    }
}
