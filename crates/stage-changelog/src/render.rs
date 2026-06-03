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
    use anodizer_core::log::{StageLogger, Verbosity};

    // Load just the changelog section from .anodizer.yaml. We deliberately
    // skip the include/deprecation machinery in `cli::pipeline::load_config`
    // so this stays usable from non-CLI contexts (it lives in core's dep graph
    // already; pulling cli in would create a cycle).
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
    // any empty heading at the start so our `## [<version>]` heading stands
    // alone.
    let body = body.trim_start_matches("## \n\n").trim_start().to_string();

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
    let has_curated = curated_body.iter().any(|l| !l.trim().is_empty());

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
    if has_curated {
        // Trim leading/trailing blank lines from the curated block but keep
        // its interior verbatim.
        let start = curated_body
            .iter()
            .position(|l| !l.trim().is_empty())
            .unwrap_or(0);
        let end = curated_body
            .iter()
            .rposition(|l| !l.trim().is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);
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
    out_lines.push(format!("[Unreleased]: {}/compare/{}...HEAD", base, new_tag));
    out_lines.push(format!(
        "[{}]: {}/compare/{}...{}",
        to_version, base, old_anchor, new_tag
    ));
    // Remaining footer lines (prior `[x.y.z]:` links) unchanged.
    out_lines.extend(tail[footer_idx + 1..].iter().map(|s| s.to_string()));

    Ok(())
}

/// Synthesize a `[Unreleased]:` / `[<version>]:` footer from the `origin`
/// remote when the KAC file lacks one. Skips gracefully (no footer appended)
/// when the previous tag or the remote cannot be resolved — a missing remote
/// must never fail the render.
fn synthesize_footer(
    out_lines: &mut Vec<String>,
    from_tag: Option<&str>,
    to_version: &str,
    workspace_root: &std::path::Path,
) {
    let Some(old_anchor) = from_tag else {
        return;
    };
    let Ok((owner, repo)) = anodizer_core::git::detect_github_repo_in(workspace_root) else {
        return;
    };
    let base = format!("https://github.com/{}/{}", owner, repo);
    let prefix = tag_prefix(old_anchor);
    let new_tag = format!("{}{}", prefix, to_version);

    // Ensure a blank line separates the body from a freshly-added footer block.
    if out_lines.last().is_some_and(|l| !l.is_empty()) {
        out_lines.push(String::new());
    }
    out_lines.push(format!("[Unreleased]: {}/compare/{}...HEAD", base, new_tag));
    out_lines.push(format!(
        "[{}]: {}/compare/{}...{}",
        to_version, base, old_anchor, new_tag
    ));
}
