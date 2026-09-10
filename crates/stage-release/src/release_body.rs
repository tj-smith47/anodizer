//! Release body / metadata helpers — composing the GitHub release body from
//! changelog + header + footer, resolving extra-file globs, mapping
//! `make_latest` config to the octocrab enum, validating release mode,
//! fetching `from_url`/`from_file` content sources, composing the final
//! body for `keep-existing` / `append` / `prepend` / `replace` modes,
//! building the create/update JSON payload, and resolving the release tag
//! template. Lifted out of the ReleaseStage monolith so the body-shape
//! decisions are reviewable in one place.

use anodizer_core::config::{ContentSource, ExtraFileSpec, MakeLatestConfig, ReleaseConfig};
use anodizer_core::context::Context;
use anyhow::{Context as _, Result};
use std::borrow::Cow;

/// Resolve header/footer precedence for the GitHub release body.
///
/// Anodizer-local precedence: `release.header` / `release.footer` is the more
/// specific override and wins; `changelog.header` / `changelog.footer` is the
/// fallback so a YAML-configured changelog wrapper still reaches the release
/// body. The changelog stage stashes the `changelog.*` value on the context;
/// the `release.*` value layers on top as a more specific override.
///
/// `release_value` is the already-rendered `release.header` / `release.footer`
/// string; `changelog_value` is the rendered `changelog.header` /
/// `changelog.footer` value stashed on the context by the changelog stage.
pub(crate) fn resolve_header_footer<'a>(
    release_value: Option<&'a str>,
    changelog_value: Option<&'a str>,
) -> Option<&'a str> {
    release_value.or(changelog_value)
}

/// Resolve the release-body footer, applying the default attribution line.
///
/// REPLACE semantics: a configured `release.footer` (or, as today's fallback,
/// `changelog.footer`) replaces [`ReleaseConfig::DEFAULT_FOOTER`] rather than
/// stacking with it. An explicit `release.footer: ""` resolves to the empty
/// string, which [`build_release_body`] drops — the documented opt-out.
pub(crate) fn resolve_release_footer<'a>(
    release_value: Option<&'a str>,
    changelog_value: Option<&'a str>,
) -> &'a str {
    resolve_header_footer(release_value, changelog_value).unwrap_or(ReleaseConfig::DEFAULT_FOOTER)
}

/// Join the derived Full-Changelog element and the footer into the single
/// trailing block [`build_release_body`] separates from the changelog with a
/// blank line.
///
/// The two are joined by ONE newline (a markdown soft break) so the shipped
/// layout — rule, link, attribution — stays a single trailing paragraph.
/// Returns `None` when neither element has content, so a body with no trailer
/// renders exactly as it did before either existed.
pub(crate) fn compose_release_trailer(link: Option<&str>, footer: &str) -> Option<String> {
    match (link, footer.is_empty()) {
        (Some(l), false) => Some(format!("{l}\n{footer}")),
        (Some(l), true) => Some(l.to_string()),
        (None, false) => Some(footer.to_string()),
        (None, true) => None,
    }
}

/// Render the derived `**Full Changelog**` element for one crate's release
/// body, or `None` when it must be omitted.
///
/// Omitted when: the crate opted out (`release.full_changelog_link: false`);
/// no previous tag exists in this release's tag family (a first release — a
/// compare URL with an empty lower bound 404s); no `release.<provider>` block
/// resolves; or `already_linked` is set because the changelog body or the
/// resolved footer already carries a `**Full Changelog**:` line.
///
/// The compare bounds are the context's `Tag` / `PreviousTag` template vars —
/// the same pair a hand-written footer used, and, under a per-crate release,
/// the crate's OWN family tags. Under a configured `monorepo.tag_prefix` those
/// two vars are prefix-stripped, so the `Prefixed*` variants (which hold the
/// real git refs) are used instead.
///
/// The repo is the one this release publishes to, NOT the `origin` remote: a
/// `release.provider:` cross-publish must link against the forge the release
/// lands on. A nightly `publish_repo` override is applied inside the GitHub
/// backend, after this runs, and is deliberately not honoured here — the tags
/// being compared live in the source repo.
pub(crate) fn full_changelog_element(
    ctx: &Context,
    release_cfg: &ReleaseConfig,
    already_linked: bool,
) -> Result<Option<String>> {
    if already_linked || !release_cfg.resolved_full_changelog_link() {
        return Ok(None);
    }
    let (tag_var, prev_var) = if ctx.config.monorepo_tag_prefix().is_some() {
        ("PrefixedTag", "PrefixedPreviousTag")
    } else {
        ("Tag", "PreviousTag")
    };
    let vars = ctx.template_vars();
    let (Some(tag), Some(prev)) = (vars.get(tag_var), vars.get(prev_var)) else {
        return Ok(None);
    };
    if tag.is_empty() || prev.is_empty() {
        return Ok(None);
    }
    let (tag, prev) = (tag.to_string(), prev.to_string());
    let Some(repo) =
        anodizer_core::download_url::resolve_release_repo(release_cfg, ctx.token_type, ctx)?
    else {
        return Ok(None);
    };
    if repo.owner.is_empty() && repo.name.is_empty() {
        return Ok(None);
    }
    let base = anodizer_core::download_url::default_download_base(ctx);
    let url =
        crate::compose_compare_url(ctx.token_type, &base, &repo.owner, &repo.name, &prev, &tag);
    Ok(Some(format!("---\n**Full Changelog**: {url}")))
}

/// The blank line every release-body join uses, whether it separates a
/// header from a changelog or an existing release body from a new one. The
/// joins and the length budgets that reserve room for them read this one
/// value, so they cannot drift apart.
pub(crate) const BODY_SEPARATOR: &str = "\n\n";

/// Line endings stripped from the end of each release-body part before the
/// join. A YAML block scalar (`header: |`) contributes `\n`; a `from_file`
/// source authored on Windows contributes `\r\n`, which `read_to_string`
/// hands over untranslated.
const PART_TRAILING_LINE_ENDINGS: [char; 2] = ['\n', '\r'];

/// Construct the release body by wrapping the changelog with optional
/// header and footer from the release config.
///
/// Each part's own trailing line endings are dropped before the join: a YAML
/// block scalar (`header: |`) carries one, and keeping it would render a
/// second blank line between the header and the changelog that the author
/// never wrote. The join owns the separation.
///
/// When the assembled body would exceed [`GITHUB_RELEASE_BODY_MAX_CHARS`],
/// only the changelog is cut, and it carries the ellipsis marker. The header
/// and the footer — which holds the derived `**Full Changelog**` link and the
/// attribution line — are reserved, so the elements anodizer itself owns are
/// never the first thing an oversized release loses.
pub(crate) fn build_release_body(
    changelog_body: &str,
    header: Option<&str>,
    footer: Option<&str>,
) -> String {
    let header = header
        .map(|h| h.trim_end_matches(PART_TRAILING_LINE_ENDINGS))
        .filter(|h| !h.is_empty());
    let footer = footer
        .map(|f| f.trim_end_matches(PART_TRAILING_LINE_ENDINGS))
        .filter(|f| !f.is_empty());
    let changelog = changelog_body.trim_end_matches(PART_TRAILING_LINE_ENDINGS);

    let assembled_len = |changelog_len: usize| -> usize {
        let count = usize::from(header.is_some())
            + usize::from(changelog_len > 0)
            + usize::from(footer.is_some());
        if count == 0 {
            return 0;
        }
        // Header / changelog / footer are separated by a blank line, and the
        // body ends in a single newline.
        header.map_or(0, str::len)
            + changelog_len
            + footer.map_or(0, str::len)
            + BODY_SEPARATOR.len() * (count - 1)
            + 1
    };

    let reserved = assembled_len(changelog.len()).saturating_sub(changelog.len());
    let changelog = if assembled_len(changelog.len()) > GITHUB_RELEASE_BODY_MAX_CHARS {
        Cow::Owned(truncate_with_ellipsis(
            changelog,
            GITHUB_RELEASE_BODY_MAX_CHARS.saturating_sub(reserved),
        ))
    } else {
        Cow::Borrowed(changelog)
    };

    let mut parts: Vec<&str> = Vec::new();
    if let Some(h) = header {
        parts.push(h);
    }
    if !changelog.is_empty() {
        parts.push(&changelog);
    }
    if let Some(f) = footer {
        parts.push(f);
    }

    if parts.is_empty() {
        String::new()
    } else {
        // Header / changelog / footer are separated by a blank line so
        // markdown renderers treat them as distinct paragraphs.
        let mut s = parts.join(BODY_SEPARATOR);
        s.push('\n');
        s
    }
}

/// The marker anodizer appends where it cut an over-long release body.
/// GoReleaser's is the same three-dot ellipsis.
const TRUNCATION_ELLIPSIS: &str = "...";

/// Cut `s` down to at most `max_len` bytes, ending it with
/// [`TRUNCATION_ELLIPSIS`], never splitting a UTF-8 character.
///
/// A `max_len` too small to hold even the marker yields an empty string —
/// nothing of the original survives, and a partial marker would read as
/// content.
fn truncate_with_ellipsis(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let Some(max_content) = max_len.checked_sub(TRUNCATION_ELLIPSIS.len()) else {
        return String::new();
    };
    let safe_end = s
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&end| end <= max_content)
        .last()
        .unwrap_or(0);
    format!("{}{}", &s[..safe_end], TRUNCATION_ELLIPSIS)
}

/// Render the "Non-deterministic exemptions:" block injected above the
/// SHA256SUMS section of the release body when the operator passes one
/// or more `--allow-nondeterministic <name>=<reason>` flags.
///
/// Empty input yields an empty string so callers can unconditionally
/// concatenate without a guard. The block ends with a single trailing
/// newline so it composes cleanly with whatever follows (typically the
/// SHA256SUMS code-fence). Output uses ASCII separators (no emdash) so
/// it renders predictably regardless of the consumer's encoding.
///
/// Shape (rendered):
///
/// ```text
/// Non-deterministic exemptions:
///   foo.rpm - tool-bug-1234
///   bar.msi - signing-cert-rotation
/// ```
pub(crate) fn render_nondeterministic_exemptions_block(entries: &[(String, String)]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::from("Non-deterministic exemptions:\n");
    for (name, reason) in entries {
        out.push_str(&format!("  {} - {}\n", name, reason));
    }
    out
}

/// Resolve `extra_files` glob patterns into concrete file paths.
/// Returns `(path, optional_rendered_name)` pairs. When a `Detailed` spec has
/// a `name_template`, the template is rendered using the provided `Context` and
/// returned as the second element; the upload loop should use this as the
/// upload filename instead of the filesystem name. Invalid glob patterns and
/// patterns that match zero files are hard errors, not silent skips — except
/// under `--dry-run`, where a zero-match downgrades to a warning (see
/// [`extra_files_zero_match`]).
pub(crate) fn collect_extra_files(
    specs: &[ExtraFileSpec],
    ctx: &Context,
) -> anyhow::Result<Vec<(std::path::PathBuf, Option<String>)>> {
    let mut results = Vec::new();
    for spec in specs {
        match spec {
            ExtraFileSpec::Glob(pattern) => {
                let entries = glob::glob(pattern).with_context(|| {
                    format!("release: invalid extra_files glob pattern '{}'", pattern)
                })?;
                let before = results.len();
                for entry in entries {
                    let entry = entry.with_context(|| {
                        format!(
                            "release: extra_files glob '{}': IO error iterating matches",
                            pattern
                        )
                    })?;
                    if entry.is_file() {
                        results.push((entry, None));
                    }
                }
                if results.len() == before {
                    extra_files_zero_match(pattern, ctx)?;
                }
            }
            ExtraFileSpec::Detailed {
                glob: pattern,
                name_template,
                allow_empty,
            } => {
                let entries = glob::glob(pattern).with_context(|| {
                    format!("release: invalid extra_files glob pattern '{}'", pattern)
                })?;
                let before = results.len();
                for entry in entries {
                    let entry = entry.with_context(|| {
                        format!(
                            "release: extra_files glob '{}': IO error iterating matches",
                            pattern
                        )
                    })?;
                    if entry.is_file() {
                        let name = match name_template.as_ref() {
                            Some(tmpl) => {
                                let filename =
                                    entry.file_name().unwrap_or_default().to_string_lossy();
                                let mut vars = ctx.template_vars().clone();
                                vars.set("ArtifactName", &filename);
                                vars.set(
                                    "ArtifactExt",
                                    anodizer_core::template::extract_artifact_ext(&filename),
                                );
                                Some(
                                    anodizer_core::template::render(tmpl, &vars).with_context(
                                        || {
                                            format!(
                                            "release: render extra_files name_template '{}' for '{}'",
                                            tmpl,
                                            entry.display()
                                        )
                                        },
                                    )?,
                                )
                            }
                            None => None,
                        };
                        results.push((entry, name));
                    }
                }
                if results.len() == before && !*allow_empty {
                    extra_files_zero_match(pattern, ctx)?;
                }
            }
        }
    }
    Ok(results)
}

/// Handle an `extra_files` glob that matched nothing. In a live or snapshot
/// run this is a hard error: a missing extra file means an upstream step
/// (e.g. a `before:` hook generating a man page) silently broke, and the
/// release must not proceed without the asset. Under `--dry-run` the hooks
/// are printed instead of executed, so hook-produced files legitimately
/// cannot exist yet — downgrade to a warning so dry-run gates stay usable
/// without forcing `allow_empty` (which would also mute the live-run check).
fn extra_files_zero_match(pattern: &str, ctx: &Context) -> Result<()> {
    if ctx.is_dry_run() {
        ctx.logger("release").warn(&format!(
            "extra_files glob '{pattern}' matched no files \
             (dry-run: hooks were not executed; a live release fails here)"
        ));
        return Ok(());
    }
    anyhow::bail!("release: extra_files glob '{pattern}' matched no files")
}

/// Convert our config's `MakeLatestConfig` into octocrab's `MakeLatest` enum.
///
/// When the config contains a template string (`MakeLatestConfig::String`), it is
/// rendered through the provided `render` function first, then resolved:
/// - `"true"` / `"1"` → `MakeLatest::True`
/// - `"false"` / `"0"` / `""` → `MakeLatest::False`
/// - `"auto"` → `MakeLatest::Legacy`
///
/// `make_latest` is rendered through the template engine at
/// publish time.
pub(crate) fn resolve_make_latest<F>(
    config: &Option<MakeLatestConfig>,
    render: F,
) -> Result<Option<octocrab::repos::releases::MakeLatest>>
where
    F: Fn(&str) -> anyhow::Result<String>,
{
    use octocrab::repos::releases::MakeLatest;
    Ok(match config {
        Some(MakeLatestConfig::Bool(true)) => Some(MakeLatest::True),
        Some(MakeLatestConfig::Bool(false)) => Some(MakeLatest::False),
        Some(MakeLatestConfig::Auto) => Some(MakeLatest::Legacy),
        Some(MakeLatestConfig::String(tmpl)) => {
            let rendered = render(tmpl)
                .with_context(|| format!("release: render make_latest template '{tmpl}'"))?;
            match rendered.trim() {
                "true" | "1" => Some(MakeLatest::True),
                "false" | "0" | "" => Some(MakeLatest::False),
                "auto" => Some(MakeLatest::Legacy),
                _ => Some(MakeLatest::True), // non-empty = truthy
            }
        }
        None => None,
    })
}

/// Resolve a `ContentSource` for the release block (header/footer/body).
/// Thin wrapper that hands off to [`anodizer_core::content_source::resolve`]
/// with a release-specific label so error messages identify the source.
pub(crate) fn resolve_content_source(
    source: &ContentSource,
    ctx: &anodizer_core::context::Context,
) -> Result<String> {
    anodizer_core::content_source::resolve(
        source,
        "release header/footer",
        ctx,
        &ctx.logger("release"),
    )
}

/// Compose the final release body based on the release mode.
///
/// - `"replace"` — use new_body as-is (current behavior)
/// - `"keep-existing"` — if existing_body is non-empty, keep it; otherwise use new_body
/// - `"append"` — append new_body after existing_body
/// - `"prepend"` — prepend new_body before existing_body
pub(crate) fn compose_body_for_mode(
    mode: &str,
    existing_body: Option<&str>,
    new_body: &str,
) -> String {
    match mode {
        "keep-existing" => {
            if let Some(existing) = existing_body
                && !existing.is_empty()
            {
                return existing.to_string();
            }
            new_body.to_string()
        }
        "append" => {
            if let Some(existing) = existing_body
                && !existing.is_empty()
            {
                // The new body ends with the derived Full-Changelog link and
                // the attribution footer, and `build_release_body` already
                // fit it under the limit. So the EXISTING half absorbs the
                // cut: truncating the joined string would land on the tail,
                // which is exactly where that trailer lives.
                let budget = GITHUB_RELEASE_BODY_MAX_CHARS
                    .saturating_sub(new_body.len() + BODY_SEPARATOR.len());
                let existing = truncate_with_ellipsis(existing, budget);
                if existing.is_empty() {
                    return new_body.to_string();
                }
                return format!("{existing}{BODY_SEPARATOR}{new_body}");
            }
            new_body.to_string()
        }
        "prepend" => {
            if let Some(existing) = existing_body
                && !existing.is_empty()
            {
                return format!("{new_body}{BODY_SEPARATOR}{existing}");
            }
            new_body.to_string()
        }
        // "replace" or any other value — just use new_body
        _ => new_body.to_string(),
    }
}

/// GitHub's maximum release body length in characters.
pub(crate) const GITHUB_RELEASE_BODY_MAX_CHARS: usize = 125_000;

/// Spec bundling every field that goes into a GitHub release JSON body.
///
/// Used by both POST (create) and PATCH (update) call sites.
/// Mirrors the fields in `GithubReleaseSpec` consumed by `run_github_backend`
/// (see `github/mod.rs`) so the create-release path can pass through a
/// borrow without intermediate copies.
#[derive(Clone, Copy)]
pub(crate) struct ReleaseJsonSpec<'a> {
    pub tag: &'a str,
    pub name: &'a str,
    pub body: &'a str,
    pub draft: bool,
    pub prerelease_flag: bool,
    pub make_latest: &'a Option<octocrab::repos::releases::MakeLatest>,
    pub target_commitish: &'a Option<String>,
    pub discussion_category: &'a Option<String>,
}

/// Build the JSON body for GitHub release create/update API calls.
/// Extracts the common construction shared by PATCH (update existing draft)
/// and POST (create new release) paths.
///
/// Note: `generate_release_notes` is intentionally never set on this
/// payload. The github-native changelog flow calls
/// `POST /repos/{o}/{r}/releases/generate-notes` upfront (see
/// `stage-changelog/src/github_native.rs`) and embeds the returned body
/// in `spec.body`. The create-release
/// `generate_release_notes: true` toggle silently uses GitHub's "most
/// recent published release" as the previous tag — wrong for monorepos
/// and tag-prefixed re-releases.
pub(crate) fn build_release_json(spec: &ReleaseJsonSpec<'_>) -> serde_json::Value {
    let ReleaseJsonSpec {
        tag,
        name,
        body,
        draft,
        prerelease_flag,
        make_latest,
        target_commitish,
        discussion_category,
    } = *spec;
    // `tag_name` is required by `POST /repos/{owner}/{repo}/releases` per
    // <https://docs.github.com/en/rest/releases/releases#create-a-release>;
    // `resolve_release_tag` bails when the resolved tag is empty, so this
    // branch is unreachable with `tag == ""`.
    // `name` is optional per the same REST docs (GitHub defaults to the
    // tag when omitted) — sending an empty string is harmless: the GH UI
    // renders the tag as the release header, and `resolved_name_template`
    // defaults to `"{{ Tag }}"` so callers practically never pass empty.
    let mut json = serde_json::json!({
        "tag_name": tag,
        "name": name,
        "draft": draft,
        "prerelease": prerelease_flag,
    });
    // `body` (description) is optional per the same Create-a-release REST
    // docs; omit the key entirely when empty so the GitHub UI shows "No
    // description provided" instead of a literal empty line above the
    // asset list.
    if !body.is_empty() {
        // The backstop for bodies composed outside `build_release_body` — the
        // `append` / `prepend` modes concatenate an existing release body with
        // a new one. A body that came through `build_release_body` already
        // fits, with its header and footer reserved.
        let truncated_body = truncate_with_ellipsis(body, GITHUB_RELEASE_BODY_MAX_CHARS);
        json["body"] = serde_json::Value::String(truncated_body);
    }
    if let Some(ml) = make_latest {
        json["make_latest"] = serde_json::Value::String(ml.to_string());
    }
    if let Some(tc) = target_commitish {
        json["target_commitish"] = serde_json::json!(tc);
    }
    if let Some(dc) = discussion_category {
        json["discussion_category_name"] = serde_json::json!(dc);
    }
    json
}

/// Build the JSON body for the un-draft (publish) PATCH on `/repos/{o}/{r}/releases/{id}`.
///
/// Publish-PATCH body composition (commits
/// `6ecba31405e8ade89b335bf05e19734d0fd8d2d8` +
/// `2e17678c4be30b1c53b5931919b57e71532b6d16`):
///
/// - Always sends `draft = false`.
/// - Re-renders the release `name` (callers pass the already-rendered template
///   value) so a stale draft created with an older name template is corrected
///   on publish.
/// - Sends `prerelease = true` when `prerelease` is set; only sends
///   the field when true (omitted == GitHub default of "preserve").
/// - Sends `make_latest = "false"` whenever `prerelease` is true, regardless of
///   the user's `make_latest` template — a prerelease cannot be the latest.
///   When `prerelease` is false, the user's `make_latest` value (if any) is
///   sent verbatim.
/// - Sends `discussion_category_name` only on publish (GitHub ignores it on
///   draft creation).
pub(crate) fn build_publish_patch_body(
    release_name: &str,
    prerelease: bool,
    make_latest: &Option<octocrab::repos::releases::MakeLatest>,
    discussion_category: &Option<String>,
) -> serde_json::Value {
    let mut body = serde_json::json!({ "draft": false });
    if !release_name.is_empty() {
        body["name"] = serde_json::Value::String(release_name.to_string());
    }
    if prerelease {
        body["prerelease"] = serde_json::Value::Bool(true);
        // Force make_latest=false for prereleases (PR
        // #6591 (commit `6ecba31...` — see PR ref above): a prerelease
        // cannot also be marked "latest", regardless of the user's
        // `make_latest` template.
        body["make_latest"] = serde_json::Value::String("false".to_string());
    } else if let Some(ml) = make_latest {
        // NB: only set `prerelease` when true:
        // an un-draft PATCH that *omits* `prerelease` leaves whatever flag
        // GitHub already has on the draft. So a stale draft created earlier
        // with `prerelease=true` whose user has since re-rendered to false
        // will keep the `prerelease=true` flag in GitHub. To clear it the
        // user must delete + recreate the draft; do NOT "fix" this by also
        // sending `prerelease=false` here.
        body["make_latest"] = serde_json::Value::String(ml.to_string());
    }
    if let Some(dc) = discussion_category {
        body["discussion_category_name"] = serde_json::json!(dc);
    }
    body
}
