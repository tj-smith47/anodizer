use super::*;

/// How a resolved GitHub login renders in the author-mention slot.
///
/// The same render path serves two sinks with different autolink behaviour:
/// a GitHub release body autolinks bare `@login` mentions itself, while a
/// committed `CHANGELOG.md` needs an explicit Markdown link to be clickable.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum LoginStyle {
    /// Bare `@login` — for GitHub release bodies, where GitHub autolinks it.
    #[default]
    Bare,
    /// `[@login](https://github.com/login)` — for on-disk Markdown.
    Linked,
}

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
    /// How resolved logins render in the author-mention slot.
    pub login_style: LoginStyle,
}

/// Compute the release-wide unique author-name set across every commit
/// in the rendered groups (including subgroups) and return it as a
/// comma-separated string. Mirrors how `AllLogins` is built upstream of
/// `render_changelog_with_provider`; surfaces as the `AllAuthors`
/// per-line template var so users can render
/// `Contributors: {{ AllAuthors }}` once at the bottom of a changelog
/// footer (the per-line scope is the only template scope anodizer
/// currently exposes from the changelog renderer).
pub(crate) fn collect_all_authors(grouped: &[GroupedCommits]) -> String {
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

/// Release-wide unique login set across every commit in the rendered groups,
/// joined with commas (the same shape the SCM compare-API fetchers produce
/// for `AllLogins`). Used when the caller supplies no fetch-time login string
/// — the local-git path, where logins arrive via GitHub-API enrichment on the
/// commits themselves rather than from a compare response.
pub(crate) fn collect_all_logins(grouped: &[GroupedCommits]) -> String {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    fn walk(group: &GroupedCommits, seen: &mut std::collections::BTreeSet<String>) {
        for commit in &group.commits {
            if !commit.login.is_empty() {
                seen.insert(commit.login.clone());
            }
        }
        for sub in &group.subgroups {
            walk(sub, seen);
        }
    }
    for g in grouped {
        walk(g, &mut seen);
    }
    seen.into_iter().collect::<Vec<_>>().join(",")
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
        login_style,
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
    // The SCM compare backends supply `logins` from their API response; the
    // local-git path supplies an empty string, so derive `AllLogins` from the
    // (possibly enriched) per-commit logins instead.
    let derived_logins;
    let logins = if logins.is_empty() {
        derived_logins = collect_all_logins(grouped);
        derived_logins.as_str()
    } else {
        logins
    };
    let state = RenderGroupsState {
        abbrev,
        tmpl,
        logins,
        all_authors: &all_authors,
        divider,
        newline,
        login_style,
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
pub(crate) struct RenderGroupsState<'a> {
    pub(crate) abbrev: i32,
    pub(crate) tmpl: &'a str,
    pub(crate) logins: &'a str,
    pub(crate) all_authors: &'a str,
    pub(crate) divider: Option<&'a str>,
    pub(crate) newline: &'a str,
    pub(crate) login_style: LoginStyle,
}

impl<'a> RenderGroupsState<'a> {
    fn with_divider(self, divider: Option<&'a str>) -> Self {
        Self { divider, ..self }
    }
}

/// Recursively render grouped commits at the given heading depth.
/// Depth is capped at 6 (matching Markdown's `######` max heading level).
pub(crate) fn render_groups(
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
            render_commit_line(out, commit, state)?;
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
/// A `* ` Markdown bullet is prepended unless the rendered line already opens
/// with a list marker (`* ` or `- `, including the tab-separated form), so a
/// user `format:` that begins with its own bullet renders one marker rather
/// than a doubled `* *`.
///
/// `AuthorUsername` renders the `@login` mention when a per-commit login is
/// known (from the SCM compare backends, or from GitHub-API enrichment of the
/// local-git path) and falls back to the commit author name otherwise, so an
/// empty `{{ .AuthorUsername }}` never renders a bare `()`. The raw `Login`
/// stays empty when unresolved so the default SCM format's
/// `{% if Login %}…{% else %}{{ AuthorName }} <{{ AuthorEmail }}>{% endif %}`
/// branch still selects the `Name <email>` form. Resolved logins additionally
/// pass through [`style_login_mentions`], which links `@login` tokens under
/// [`LoginStyle::Linked`] (on-disk changelogs).
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
/// - `Login` — per-commit SCM username (populated by the `github`/`gitea`
///   backends, or by GitHub-API enrichment when the local-git path targets a
///   GitHub repo; left empty when unresolved so the default SCM format can
///   branch on it)
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
pub(crate) fn render_commit_line(
    out: &mut String,
    commit: &CommitInfo,
    state: &RenderGroupsState<'_>,
) -> Result<()> {
    let &RenderGroupsState {
        abbrev,
        tmpl,
        logins,
        all_authors,
        newline,
        login_style,
        divider: _,
    } = state;
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
    // Every free-text input is stripped of the mention sentinel before it can
    // reach the template, so the only sentinel spans in the rendered line are
    // the ones this function substitutes — coincidental `@login` text in a
    // commit subject can never be mistaken for an author mention, and a
    // crafted sentinel in a commit message can never trigger styling.
    let description = strip_mention_sentinels(&commit.description);
    let author_name = strip_mention_sentinels(&commit.author_name);
    let author_email = strip_mention_sentinels(&commit.author_email);
    let login = strip_mention_sentinels(&commit.login);
    let mut vars = TemplateVars::new();
    // SHA respects the `abbrev` config.
    // `short_sha` is already computed with abbrev applied above; use it here
    // so templates referencing {{ .SHA }} honor the user's abbreviation.
    vars.set("SHA", &short_sha);
    vars.set("ShortSHA", &short_sha);
    vars.set("Message", &description);
    vars.set("AuthorName", &author_name);
    vars.set("AuthorEmail", &author_email);
    // `Login` stays the raw backend datum (empty unless an SCM backend or
    // login enrichment resolved a username): the default SCM format branches
    // on it (`{% if Login %}{{ AuthorUsername }}{% else %}{{ AuthorName }}
    // <{{ AuthorEmail }}>{% endif %}`), so an empty `Login` is the signal
    // that drives the `Name <email>` fallback — overwriting it would
    // suppress the email.
    vars.set("Login", &login);
    // `AuthorUsername` is the DISPLAY alias: with a resolved login it carries
    // the `@login` mention form, framed in sentinels so the post-render pass
    // styles exactly this renderer-substituted span (a GitHub release body
    // autolinks the bare mention; the Linked style turns it into an explicit
    // Markdown link for on-disk changelogs). With no login it falls back to
    // the author name so a `({{ .AuthorUsername }})` reference never renders
    // a bare `()`. Unlike the raw `Login`, the mention form means templates
    // ported from the `{{ with .AuthorUsername }}@{{ . }}{{ end }}`
    // convention would double the `@` — `style_login_mentions` collapses the
    // literal `@` directly before the span.
    let mention;
    let author_username = if login.is_empty() {
        author_name.as_ref()
    } else {
        mention = format!("{MENTION_SENTINEL}@{login}{MENTION_SENTINEL}");
        mention.as_str()
    };
    vars.set("AuthorUsername", author_username);
    // Per-entry `Authors` and `Logins` template vars: each entry gets its
    // own commit-author + co-author list. The release-wide GitHub login
    // list lives under `AllLogins` so `Logins` can carry the per-commit
    // semantic. Co-author entries (parsed from `Co-Authored-By:` trailers)
    // carry both bare name and "Name <email>" form; the raw trailer payload
    // is surfaced as the Authors join target.
    //
    // `Authors` (comma-string, backward-compatible) and `AuthorsList`
    // (structured {Name, Email, Username} records for
    // `{% for a in AuthorsList %}@{{ a.Username }}{% endfor %}`) are built
    // from a SINGLE walk over the author + co-author set: the comma-string
    // is derived from the structured list's names rather than re-iterating.
    // Co-author trailers contribute Name only (email is in the raw trailer
    // string; Username is unknown without an extra SCM API hit — left empty).
    let mut authors_list: Vec<JsonValue> = Vec::new();
    if !author_name.is_empty() {
        let mut obj = serde_json::Map::new();
        obj.insert("Name".into(), JsonValue::String(author_name.to_string()));
        obj.insert("Email".into(), JsonValue::String(author_email.to_string()));
        obj.insert("Username".into(), JsonValue::String(login.to_string()));
        authors_list.push(JsonValue::Object(obj));
    }
    for ca in &commit.co_authors {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "Name".into(),
            JsonValue::String(strip_mention_sentinels(ca).into_owned()),
        );
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
    if !login.is_empty() {
        commit_logins.push(login.to_string());
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
    vars.set("AllLogins", &strip_mention_sentinels(logins));
    vars.set("AllAuthors", &strip_mention_sentinels(all_authors));
    let rendered = template::render(tmpl, &vars).with_context(|| {
        format!(
            "changelog: render commit format template '{tmpl}' for commit {}",
            commit.hash
        )
    })?;
    let rendered = if login.is_empty() {
        // No login resolved: no sentinel span was substituted, so the line is
        // byte-identical to the historical name-based rendering. The strip is
        // a defensive no-op (inputs are sanitized above) that guarantees the
        // sentinel can never leak on this path either.
        match strip_mention_sentinels(&rendered) {
            std::borrow::Cow::Borrowed(_) => rendered,
            std::borrow::Cow::Owned(stripped) => stripped,
        }
    } else {
        style_login_mentions(&rendered, &login, login_style)
    };
    // De-dupe the leading bullet: when the user's `format:` already opens with
    // a Markdown list marker (`* ` / `- `, or the tab-separated form), emit it
    // verbatim instead of prepending a second `* ` (which yielded `* *`). The
    // default format carries no marker and so still gets its `* `.
    let starts_with_bullet = {
        let trimmed = rendered.trim_start();
        trimmed.starts_with("* ")
            || trimmed.starts_with("- ")
            || trimmed.starts_with("*\t")
            || trimmed.starts_with("-\t")
    };
    if starts_with_bullet {
        out.push_str(&format!("{}{}", rendered, newline));
    } else {
        out.push_str(&format!("* {}{}", rendered, newline));
    }
    Ok(())
}

/// Frames the renderer-substituted author mention inside the rendered line so
/// the post-render styling pass can target exactly that span. `U+0001` is a
/// control character that cannot legitimately appear in commit metadata; every
/// free-text input is stripped of it before templating
/// ([`strip_mention_sentinels`]), so a span can only originate from the
/// renderer's own `AuthorUsername` substitution — never from coincidental or
/// crafted text.
pub(crate) const MENTION_SENTINEL: char = '\u{1}';

/// Remove [`MENTION_SENTINEL`] characters from a free-text input before it is
/// handed to the template engine. Borrows when the input is already clean
/// (the overwhelmingly common case), so the byte-identical fallback contract
/// costs no allocation.
pub(crate) fn strip_mention_sentinels(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains(MENTION_SENTINEL) {
        std::borrow::Cow::Owned(s.replace(MENTION_SENTINEL, ""))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Post-render pass over one commit line for a RESOLVED `login`.
///
/// Styles exactly the sentinel-framed mention span the renderer substituted
/// for `AuthorUsername` — free text mentioning `@login` is untouchable by
/// construction (inputs are sentinel-stripped before templating):
///
/// 1. Consumes a literal `@` directly before the span: a template that
///    prefixes its own `@` (the `{{ with .AuthorUsername }}@{{ . }}{{ end }}`
///    convention) would otherwise double it.
/// 2. Replaces the span with the styled mention — bare `@login` under
///    [`LoginStyle::Bare`] (GitHub autolinks it in release bodies), or
///    `[@login](https://github.com/login)` under [`LoginStyle::Linked`] for
///    on-disk Markdown.
/// 3. Strips any residual sentinel so it can never leak: a template filter
///    (`upper` / `slice` / ...) applied to `AuthorUsername` can mangle a span
///    past recognition, in which case the mention degrades to its unstyled
///    text rather than emitting control characters.
pub(crate) fn style_login_mentions(line: &str, login: &str, style: LoginStyle) -> String {
    let span = format!("{MENTION_SENTINEL}@{login}{MENTION_SENTINEL}");
    let styled = match style {
        LoginStyle::Bare => format!("@{login}"),
        LoginStyle::Linked => format!("[@{login}](https://github.com/{login})"),
    };
    let line = line.replace(&format!("@{span}"), &span);
    let line = line.replace(&span, &styled);
    if line.contains(MENTION_SENTINEL) {
        line.replace(MENTION_SENTINEL, "")
    } else {
        line
    }
}
