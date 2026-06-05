//! Release body / metadata helpers — composing the GitHub release body from
//! changelog + header + footer, resolving extra-file globs, mapping
//! `make_latest` config to the octocrab enum, validating release mode,
//! fetching `from_url`/`from_file` content sources, composing the final
//! body for `keep-existing` / `append` / `prepend` / `replace` modes,
//! building the create/update JSON payload, and resolving the release tag
//! template. Lifted out of the ReleaseStage monolith so the body-shape
//! decisions are reviewable in one place.

use anodizer_core::config::{ContentSource, ExtraFileSpec, MakeLatestConfig};
use anodizer_core::context::Context;
use anyhow::{Context as _, Result};

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

/// Construct the release body by wrapping the changelog with optional
/// header and footer from the release config.
pub(crate) fn build_release_body(
    changelog_body: &str,
    header: Option<&str>,
    footer: Option<&str>,
) -> String {
    let mut parts: Vec<&str> = Vec::new();

    if let Some(h) = header
        && !h.is_empty()
    {
        parts.push(h);
    }

    if !changelog_body.is_empty() {
        parts.push(changelog_body);
    }

    if let Some(f) = footer
        && !f.is_empty()
    {
        parts.push(f);
    }

    if parts.is_empty() {
        String::new()
    } else {
        // Header / changelog / footer are separated by a blank line so
        // markdown renderers treat them as distinct paragraphs.
        let mut s = parts.join("\n\n");
        s.push('\n');
        s
    }
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
/// patterns that match zero files are hard errors, not silent skips.
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
                    anyhow::bail!("release: extra_files glob '{}' matched no files", pattern);
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
                    anyhow::bail!("release: extra_files glob '{}' matched no files", pattern);
                }
            }
        }
    }
    Ok(results)
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
    anodizer_core::content_source::resolve(source, "release header/footer", ctx)
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
                return format!("{}\n\n{}", existing, new_body);
            }
            new_body.to_string()
        }
        "prepend" => {
            if let Some(existing) = existing_body
                && !existing.is_empty()
            {
                return format!("{}\n\n{}", new_body, existing);
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
        let truncated_body = if body.len() > GITHUB_RELEASE_BODY_MAX_CHARS {
            // Truncation marker —
            //     ellipsis = "..."
            // Anodizer previously appended `"\n\n...(truncated)"` (16 chars);
            // a literal three-dot ellipsis.
            let suffix = "...";
            let max_content = GITHUB_RELEASE_BODY_MAX_CHARS - suffix.len();
            let safe_end = body
                .char_indices()
                .map(|(i, c)| i + c.len_utf8())
                .take_while(|&end| end <= max_content)
                .last()
                .unwrap_or(0);
            format!("{}{}", &body[..safe_end], suffix)
        } else {
            body.to_string()
        };
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

/// Resolve the GitHub release tag for a crate.
///
/// If `release_tag_override` is `Some`, render it as a template and use the
/// result.  Otherwise, render `tag_template`.  This implements the
/// Pro `release.tag` override behaviour.
///
/// Bails when the rendered tag is empty: the GitHub / GitLab / Gitea Releases
/// REST APIs all require a non-empty `tag_name`, and silently POSTing
/// `tag_name: ""` returns a confusing 422 (`tag_name is too short`) that
/// hides the real cause (template rendered to empty because a referenced
/// variable was missing on the snapshot path).
pub(crate) fn resolve_release_tag(
    ctx: &Context,
    tag_template: &str,
    release_tag_override: Option<&str>,
    crate_name: &str,
) -> Result<String> {
    let (rendered, source) = if let Some(override_tmpl) = release_tag_override {
        let rendered = ctx.render_template(override_tmpl).with_context(|| {
            format!(
                "release: render release.tag override for crate '{}'",
                crate_name
            )
        })?;
        (rendered, "release.tag")
    } else {
        let rendered = ctx
            .render_template(tag_template)
            .with_context(|| format!("release: render tag_template for crate '{}'", crate_name))?;
        (rendered, "tag_template")
    };
    if rendered.is_empty() {
        anyhow::bail!(
            "release: {} for crate '{}' rendered to an empty tag. The GitHub / \
             GitLab / Gitea Releases REST API requires a non-empty `tag_name`; \
             posting an empty value returns a confusing 422 (`tag_name is too \
             short`) that hides the real cause. Verify the template references \
             a variable that is populated on this run (e.g. `{{{{ Tag }}}}` is \
             unset during `--snapshot` without a `tag_template` fallback) or \
             set an explicit `release.tag:` override.",
            source,
            crate_name
        );
    }
    Ok(rendered)
}
