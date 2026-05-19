//! Markdown rendering and the `render_crate_section` public entry point.
//!
//! `render_changelog_with_provider` is the canonical render function
//! (used by both the in-pipeline `Stage::run` body and the
//! `bump --commit` flow in `render_crate_section`). The recursive
//! `render_groups` walks the `GroupedCommits` tree to produce headings +
//! bullets at the configured Markdown depth.
//!
//! `merge_into_changelog` is the file-level merge helper used by
//! `render_crate_section` to splice a new `## [<version>]` section into
//! an existing `CHANGELOG.md` while preserving the leading H1.

use anodizer_core::config::ChangelogGroup;
use anodizer_core::template::{self, TemplateVars};
use anyhow::{Context as _, Result};

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
    /// GoReleaser's `newLineFor()` which inspects `ctx.TokenType`).
    pub scm_provider: Option<&'a str>,
}

/// Inner render function that accepts an optional SCM provider override for
/// newline handling. GoReleaser's `newLineFor()` checks `ctx.TokenType`, not
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
    // GoReleaser's newLineFor() checks ctx.TokenType, not the changelog source.
    // See https://docs.gitlab.com/ee/user/markdown.html#newlines
    let nl_source = scm_provider.unwrap_or(use_source);
    let newline = match nl_source {
        "gitlab" | "gitea" => "   \n",
        _ => "\n",
    };
    let mut out = String::new();
    // Title heading. Three states:
    //   - title == None        → emits `## Changelog` (GoReleaser-equivalent default).
    //   - title == Some("foo") → emits `## foo`.
    //   - title == Some("")    → suppresses the heading entirely (anodize-additive
    //                            UX win — see divergence note below).
    //
    // Divergence from GoReleaser (intentional carve-out): GoReleaser emits the
    // `## Changelog` heading unconditionally and offers no way to suppress it.
    // Anodizer treats an explicit empty `title:` as a request to omit the
    // heading, which lets users compose changelogs into other surfaces (release
    // bodies, RSS feeds, embedded docs) without a redundant heading. Default
    // behaviour still matches GoReleaser; the carve-out is opt-in.
    let changelog_title = title.unwrap_or(ChangelogConfig::DEFAULT_TITLE);
    if !changelog_title.is_empty() {
        out.push_str(&format!("## {}\n\n", changelog_title));
    }
    let state = RenderGroupsState {
        abbrev,
        tmpl,
        logins,
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
        // spurious heading — matching GoReleaser behaviour.
        if !group.title.is_empty() {
            out.push_str(&format!("{} {}\n\n", hashes, group.title));
        }
        for commit in &group.commits {
            render_commit_line(
                out,
                commit,
                state.abbrev,
                state.tmpl,
                state.logins,
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
/// - `AuthorName` — commit author name
/// - `AuthorEmail` — commit author email
/// - `Login` — per-commit GitHub username (populated only with `github` backend)
/// - `Authors` — comma-separated names for this commit (primary author +
///   `Co-Authored-By:` trailers). matches GR's per-entry
///   Authors template var.
/// - `Logins` — comma-separated logins for this commit (primary author's
///   login + parsed co-author logins, where the trailer carries one).
///   Matches GR's per-entry Logins template var.
/// - `AllLogins` — comma-separated list of *all* GitHub logins seen in the
///   release. Was the previous `Logins` semantic (release-wide) before
///   `Logins` was reclaimed for per-entry data; renamed to keep both
///   available without ambiguity.
fn render_commit_line(
    out: &mut String,
    commit: &CommitInfo,
    abbrev: i32,
    tmpl: &str,
    logins: &str,
    newline: &str,
) -> Result<()> {
    let short_sha = if abbrev < 0 {
        // Negative abbrev (e.g. GoReleaser's -1) means omit hash entirely.
        String::new()
    } else if abbrev == 0 {
        // abbrev 0 means full SHA (no truncation).
        commit.full_hash.clone()
    } else {
        let a = abbrev as usize;
        if commit.hash.len() > a {
            commit.hash[..a].to_string()
        } else {
            commit.hash.clone()
        }
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
    // GR-aligned alias: the upstream default format string when
    // `use ∈ {github,gitlab,gitea}` is
    // `"{{ .SHA }}: {{ .Message }} ({{ with .AuthorUsername }}@{{ . }}{{ else }}{{ .AuthorName }} <{{ .AuthorEmail }}>{{ end }})"`
    // (`internal/pipe/changelog/changelog.go:59,259-271`). GR populates the
    // `AuthorUsername` template var from the SCM commit author's username;
    // anodizer surfaces the same datum under `Login`. Bind both keys so
    // GR-shape configs copy-paste cleanly without a "missing key" error.
    vars.set("AuthorUsername", &commit.login);
    // Per-entry `Authors` and `Logins` template vars: each entry gets its
    // own commit-author + co-author list. The release-wide GitHub login
    // list lives under `AllLogins` so `Logins` can carry the per-commit
    // semantic. Co-author entries (parsed from `Co-Authored-By:` trailers)
    // carry both bare name and "Name <email>" form; we surface their raw
    // trailer payload as the Authors join target.
    let mut commit_authors: Vec<String> = Vec::new();
    if !commit.author_name.is_empty() {
        commit_authors.push(commit.author_name.clone());
    }
    for ca in &commit.co_authors {
        commit_authors.push(ca.clone());
    }
    vars.set("Authors", &commit_authors.join(", "));
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
    vars.set("AllLogins", logins);
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
    let merged = merge_into_changelog(&file_path, crate_name, &new_section)?;

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
fn merge_into_changelog(
    file_path: &std::path::Path,
    crate_name: &str,
    new_section: &str,
) -> Result<String> {
    let header = format!("# Changelog — {}\n\n", crate_name);
    if !file_path.is_file() {
        return Ok(format!("{}{}", header, new_section));
    }
    let existing = std::fs::read_to_string(file_path)
        .with_context(|| format!("failed to read {}", file_path.display()))?;
    // Find the H1 line so we can preserve any prelude (license badge, etc.)
    // and append our section right after the leading header block.
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
