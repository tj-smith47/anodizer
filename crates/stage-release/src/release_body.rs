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

/// Resolve `extra_files` glob patterns into concrete file paths.
/// Returns `(path, optional_rendered_name)` pairs. When a `Detailed` spec has
/// a `name_template`, the template is rendered using the provided `Context` and
/// returned as the second element; the upload loop should use this as the
/// upload filename instead of the filesystem name.
/// invalid glob patterns
/// and patterns that match zero files are hard errors, not silent skips.
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
                for entry in entries.flatten() {
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
            } => {
                let entries = glob::glob(pattern).with_context(|| {
                    format!("release: invalid extra_files glob pattern '{}'", pattern)
                })?;
                let before = results.len();
                for entry in entries.flatten() {
                    if entry.is_file() {
                        let name = name_template.as_ref().and_then(|tmpl| {
                            let filename = entry.file_name().unwrap_or_default().to_string_lossy();
                            let mut vars = ctx.template_vars().clone();
                            vars.set("ArtifactName", &filename);
                            vars.set(
                                "ArtifactExt",
                                anodizer_core::template::extract_artifact_ext(&filename),
                            );
                            anodizer_core::template::render(tmpl, &vars).ok()
                        });
                        results.push((entry, name));
                    }
                }
                if results.len() == before {
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
/// This matches GoReleaser, which renders `make_latest` through `tmpl.Apply` at
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
                _ => Some(MakeLatest::True), // non-empty = truthy, matching GoReleaser
            }
        }
        None => None,
    })
}

/// The valid release `mode` values that control how existing release notes
/// are handled when a release already exists.
const VALID_RELEASE_MODES: &[&str] = &["keep-existing", "append", "prepend", "replace"];

/// Resolve and validate the release mode from config.
/// Returns `"keep-existing"` when `None` or empty (matches GoReleaser default).
pub(crate) fn resolve_release_mode(mode: Option<&str>) -> Result<String> {
    match mode {
        None | Some("") => Ok("keep-existing".to_string()),
        Some(m) => {
            if VALID_RELEASE_MODES.contains(&m) {
                Ok(m.to_string())
            } else {
                anyhow::bail!(
                    "release: invalid mode '{}', must be one of: {}",
                    m,
                    VALID_RELEASE_MODES.join(", ")
                );
            }
        }
    }
}

/// Resolve a `ContentSource` to its string content.
///
/// - Inline: returns the string directly.
/// - FromFile: template-renders the path, reads the file from disk.
/// - FromUrl: template-renders URL and header values, fetches via HTTP GET
///   with the supplied headers and retries (3 attempts, 500ms * 2^n backoff)
///   on transient network errors and 5xx responses. 4xx responses fail fast.
///
/// GoReleaser Pro parity: header/footer from_url supports `headers:` map for
/// authenticated private mirrors; URL and both sides of the headers map are
/// template-rendered.
pub(crate) fn resolve_content_source(
    source: &ContentSource,
    ctx: &anodizer_core::context::Context,
) -> Result<String> {
    match source {
        ContentSource::Inline(s) => Ok(s.clone()),
        ContentSource::FromFile { from_file } => {
            let rendered_path = ctx
                .render_template(from_file)
                .with_context(|| format!("render from_file path: {}", from_file))?;
            std::fs::read_to_string(&rendered_path)
                .map_err(|e| anyhow::anyhow!("failed to read {}: {}", rendered_path, e))
        }
        ContentSource::FromUrl { from_url, headers } => {
            let rendered_url = ctx
                .render_template(from_url)
                .with_context(|| format!("render from_url: {}", from_url))?;

            // Render header values (keys are literal per GoReleaser docs).
            // Reject `\r`/`\n` anywhere in a rendered value — a template
            // interpolating user-tainted data could otherwise inject a new
            // header line (CRLF injection). Also reject in literal keys as
            // defense-in-depth.
            let mut rendered_headers: Vec<(String, String)> = Vec::new();
            if let Some(map) = headers {
                for (k, v) in map {
                    if k.contains('\r') || k.contains('\n') {
                        anyhow::bail!(
                            "release from_url header key contains CR/LF (possible injection): {:?}",
                            k
                        );
                    }
                    let rendered_v = ctx.render_template(v).with_context(|| {
                        format!("render header value for '{}' at URL {}", k, rendered_url)
                    })?;
                    if rendered_v.contains('\r') || rendered_v.contains('\n') {
                        anyhow::bail!(
                            "release from_url header '{}' rendered to a value containing \
                             CR/LF (possible injection): {:?}",
                            k,
                            rendered_v
                        );
                    }
                    rendered_headers.push((k.clone(), rendered_v));
                }
            }

            let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(30))?;

            // Retry: 3 attempts, 500ms base, 2s cap. Retry on request errors +
            // 5xx; bail immediately on 4xx via ControlFlow::Break.
            use anodizer_core::retry::{RetryPolicy, retry_sync};
            use std::ops::ControlFlow;
            const POLICY: RetryPolicy = RetryPolicy {
                max_attempts: 3,
                base_delay: std::time::Duration::from_millis(500),
                max_delay: std::time::Duration::from_secs(2),
            };
            retry_sync(&POLICY, |attempt| {
                let mut req = client.get(&rendered_url);
                for (k, v) in &rendered_headers {
                    req = req.header(k.as_str(), v.as_str());
                }
                match req.send() {
                    Ok(response) => {
                        let status = response.status();
                        if status.is_success() {
                            // Enforce a 256 KiB body cap on from_url responses
                            // so a runaway server can't exhaust memory — release
                            // header/footer bodies are small markdown snippets.
                            const MAX_BODY: usize = 256 * 1024;
                            match response.bytes() {
                                Ok(bytes) => {
                                    if bytes.len() > MAX_BODY {
                                        return Err(ControlFlow::Break(anyhow::anyhow!(
                                            "from_url {} body is {} bytes, exceeds \
                                             {} KiB limit",
                                            rendered_url,
                                            bytes.len(),
                                            MAX_BODY / 1024,
                                        )));
                                    }
                                    match String::from_utf8(bytes.to_vec()) {
                                        Ok(text) => Ok(text),
                                        Err(e) => Err(ControlFlow::Break(anyhow::anyhow!(e))),
                                    }
                                }
                                Err(e) => Err(ControlFlow::Break(anyhow::anyhow!(e))),
                            }
                        } else if status.is_client_error() {
                            Err(ControlFlow::Break(anyhow::anyhow!(
                                "content URL {} returned HTTP {} (no retry on 4xx)",
                                rendered_url,
                                status
                            )))
                        } else {
                            Err(ControlFlow::Continue(anyhow::anyhow!(
                                "content URL {} returned HTTP {} (attempt {}/{})",
                                rendered_url,
                                status,
                                attempt,
                                POLICY.max_attempts
                            )))
                        }
                    }
                    Err(e) => Err(ControlFlow::Continue(anyhow::anyhow!(
                        "fetch {} failed (attempt {}/{}): {}",
                        rendered_url,
                        attempt,
                        POLICY.max_attempts,
                        e
                    ))),
                }
            })
        }
    }
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

/// Build the JSON body for GitHub release create/update API calls.
/// Extracts the common construction shared by PATCH (update existing draft)
/// and POST (create new release) paths.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_release_json(
    tag: &str,
    name: &str,
    body: &str,
    draft: bool,
    prerelease_flag: bool,
    make_latest: &Option<octocrab::repos::releases::MakeLatest>,
    target_commitish: &Option<String>,
    discussion_category: &Option<String>,
    github_native: bool,
) -> serde_json::Value {
    let mut json = serde_json::json!({
        "tag_name": tag,
        "name": name,
        "draft": draft,
        "prerelease": prerelease_flag,
    });
    if !body.is_empty() {
        let truncated_body = if body.len() > GITHUB_RELEASE_BODY_MAX_CHARS {
            let suffix = "\n\n...(truncated)";
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
    if github_native {
        json["generate_release_notes"] = serde_json::Value::Bool(true);
    }
    json
}

/// Resolve the GitHub release tag for a crate.
///
/// If `release_tag_override` is `Some`, render it as a template and use the
/// result.  Otherwise, render `tag_template`.  This implements the GoReleaser
/// Pro `release.tag` override behaviour.
pub(crate) fn resolve_release_tag(
    ctx: &Context,
    tag_template: &str,
    release_tag_override: Option<&str>,
    crate_name: &str,
) -> Result<String> {
    if let Some(override_tmpl) = release_tag_override {
        ctx.render_template(override_tmpl).with_context(|| {
            format!(
                "release: render release.tag override for crate '{}'",
                crate_name
            )
        })
    } else {
        ctx.render_template(tag_template)
            .with_context(|| format!("release: render tag_template for crate '{}'", crate_name))
    }
}
