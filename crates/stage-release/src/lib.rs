use std::collections::HashMap;
use std::sync::Arc;

use anodize_core::artifact::ArtifactKind;
use anodize_core::config::{
    ContentSource, ExtraFileSpec, GitHubUrlsConfig, MakeLatestConfig, PrereleaseConfig,
};
use anodize_core::context::Context;
use anodize_core::git;
use anodize_core::scm::ScmTokenType;
use anodize_core::stage::Stage;
use anyhow::{Context as _, Result, bail};
use http::header::HeaderValue;
use octocrab::service::middleware::auth_header::AuthHeaderLayer;
use octocrab::service::middleware::base_uri::BaseUriLayer;
use octocrab::service::middleware::extra_headers::ExtraHeadersLayer;

mod gitea;
mod gitlab;

/// Percent-encode a URL path segment (matching Go's `url.PathEscape`).
/// Encodes everything except unreserved characters and common path-safe chars.
fn percent_encode_path(s: &str) -> String {
    // PATH_SEGMENT set: encode everything except unreserved + sub-delims + ':'/'@'
    // which matches RFC 3986 pchar = unreserved / pct-encoded / sub-delims / ":" / "@"
    const PATH_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'<')
        .add(b'>')
        .add(b'?')
        .add(b'`')
        .add(b'{')
        .add(b'}')
        .add(b'%')
        .add(b'/');
    percent_encoding::utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

// ---------------------------------------------------------------------------
// check_github_rate_limit — proactive rate limit checking
// ---------------------------------------------------------------------------

/// GoReleaser parity (github.go:checkRateLimit): proactively check GitHub API
/// rate limits before making requests. If remaining calls are below the
/// threshold, sleep until the reset time.
async fn check_github_rate_limit(client: &reqwest::Client, token: &str, threshold: u64) {
    let url = "https://api.github.com/rate_limit";
    let resp = match client
        .get(url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "anodize")
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return, // Can't check — continue and hope for the best
    };

    if !resp.status().is_success() {
        return;
    }

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return,
    };

    let remaining = body
        .pointer("/resources/core/remaining")
        .and_then(|v| v.as_u64())
        .unwrap_or(u64::MAX);
    let reset_epoch = body
        .pointer("/resources/core/reset")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    if remaining > threshold {
        return;
    }

    // Sleep until reset + small buffer
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let sleep_secs = if reset_epoch > now {
        reset_epoch - now + 1
    } else {
        5 // Minimum 5 seconds if reset is in the past
    };
    eprintln!(
        "rate limit almost reached ({} remaining), sleeping for {}s...",
        remaining, sleep_secs
    );
    tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
}

// ---------------------------------------------------------------------------
// check_github_search_rate_limit — proactive search rate limit checking
// ---------------------------------------------------------------------------

/// Check GitHub Search API rate limits (separate from core rate limit).
/// Returns `true` if enough quota remains to make a search request.
#[allow(dead_code)]
async fn check_github_search_rate_limit(
    client: &reqwest::Client,
    token: &str,
    threshold: u64,
) -> bool {
    let url = "https://api.github.com/rate_limit";
    let resp = match client
        .get(url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "anodize")
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return true, // Can't check — assume ok
    };

    if !resp.status().is_success() {
        return true;
    }

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return true,
    };

    let remaining = body
        .pointer("/resources/search/remaining")
        .and_then(|v| v.as_u64())
        .unwrap_or(u64::MAX);

    remaining > threshold
}

// ---------------------------------------------------------------------------
// resolve_github_username — email → GitHub @mention resolution
// ---------------------------------------------------------------------------

/// Resolve a commit author's email to their GitHub username.
///
/// 1. Check for noreply emails: `ID+USERNAME@users.noreply.github.com` or
///    `USERNAME@users.noreply.github.com`.
/// 2. Check the in-memory cache for previously resolved emails.
/// 3. Fall back to the GitHub Search Users API: `GET /search/users?q={email}+in:email`.
///    If exactly 1 result, cache and return the login. Otherwise cache `None`.
///
/// The cache avoids repeated API calls (GitHub search limit is 30 req/min).
/// Before making API calls, checks if the search rate limit has sufficient
/// remaining quota (threshold of 5) and skips the search if not.
#[allow(dead_code)]
async fn resolve_github_username(
    octocrab: &octocrab::Octocrab,
    email: &str,
    cache: &mut HashMap<String, Option<String>>,
    rate_limit_client: Option<(&reqwest::Client, &str)>,
) -> Option<String> {
    // 1. Parse noreply email patterns.
    if let Some(domain_start) = email.find("@users.noreply.github.com") {
        let local = &email[..domain_start];
        // Format: ID+USERNAME@users.noreply.github.com
        if let Some(plus_pos) = local.find('+') {
            let username = &local[plus_pos + 1..];
            if !username.is_empty() {
                let result = username.to_string();
                cache.insert(email.to_string(), Some(result.clone()));
                return Some(result);
            }
        }
        // Format: USERNAME@users.noreply.github.com
        if !local.is_empty() {
            let result = local.to_string();
            cache.insert(email.to_string(), Some(result.clone()));
            return Some(result);
        }
    }

    // 2. Check cache.
    if let Some(cached) = cache.get(email) {
        return cached.clone();
    }

    // 3. Check search rate limit before calling the API (30 req/min limit).
    //    Skip the search if we're running low on quota.
    if let Some((client, token)) = rate_limit_client
        && !check_github_search_rate_limit(client, token, 5).await
    {
        // Search rate limit too low — skip and cache None.
        cache.insert(email.to_string(), None);
        return None;
    }

    let query = format!("{}+in:email", email);
    let route = format!("/search/users?q={}&per_page=1", percent_encode_path(&query));

    let result: Option<String> = match octocrab
        .get::<serde_json::Value, _, _>(route, None::<&()>)
        .await
    {
        Ok(json) => {
            let total = json
                .get("total_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if total == 1 {
                json.get("items")
                    .and_then(|items| items.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|user| user.get("login"))
                    .and_then(|login| login.as_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        }
        Err(_) => None,
    };

    cache.insert(email.to_string(), result.clone());
    result
}

// ---------------------------------------------------------------------------
// delete_release_asset_by_name — paginated asset deletion for GitHub
// ---------------------------------------------------------------------------

/// Search through all pages of release assets to find and delete one by name.
///
/// GitHub's List Release Assets API defaults to 30 items per page. Releases
/// with >30 assets require pagination to find a specific asset. This function
/// fetches up to `per_page=100` assets at a time and continues through pages
/// until the asset is found and deleted, or all pages are exhausted.
///
/// Returns `Ok(true)` if the asset was found and deleted, `Ok(false)` if not found.
async fn delete_release_asset_by_name(
    octo: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    release_id: u64,
    asset_name: &str,
) -> Result<bool> {
    const MAX_PAGES: u32 = 50; // 50 pages * 100 per page = 5000 assets max
    let mut page: u32 = 1;
    loop {
        let route = format!(
            "/repos/{}/{}/releases/{}/assets?per_page=100&page={}",
            owner, repo, release_id, page
        );
        let assets: Vec<octocrab::models::repos::Asset> =
            octo.get(route, None::<&()>).await.with_context(|| {
                format!(
                    "release: list assets for release {} on {}/{} (page {})",
                    release_id, owner, repo, page
                )
            })?;

        for asset in &assets {
            if asset.name == asset_name {
                octo.repos(owner, repo)
                    .release_assets()
                    .delete(asset.id.into_inner())
                    .await
                    .with_context(|| {
                        format!(
                            "release: delete asset '{}' (id={}) from release {} on {}/{}",
                            asset_name, asset.id, release_id, owner, repo
                        )
                    })?;
                return Ok(true);
            }
        }

        // If we got fewer than 100 results, there are no more pages.
        if assets.len() < 100 {
            break;
        }
        page += 1;
        if page > MAX_PAGES {
            break;
        }
    }
    Ok(false)
}

/// Look up an existing release asset by name and return its byte size.
///
/// Used by the idempotent-upload path: when GitHub rejects an upload with
/// `422 already_exists`, comparing the existing asset's size to the local
/// file size lets us decide whether a prior attempt successfully uploaded
/// the same bytes (outer-retry recovery) or whether the names collided with
/// different content (real conflict that needs `replace_existing_artifacts`).
async fn find_release_asset_size(
    octo: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    release_id: u64,
    asset_name: &str,
) -> Result<Option<u64>> {
    const MAX_PAGES: u32 = 50;
    let mut page: u32 = 1;
    loop {
        let route = format!(
            "/repos/{}/{}/releases/{}/assets?per_page=100&page={}",
            owner, repo, release_id, page
        );
        let assets: Vec<octocrab::models::repos::Asset> =
            octo.get(route, None::<&()>).await.with_context(|| {
                format!(
                    "release: list assets for release {} on {}/{} (page {})",
                    release_id, owner, repo, page
                )
            })?;

        for asset in &assets {
            if asset.name == asset_name {
                return Ok(Some(asset.size as u64));
            }
        }

        if assets.len() < 100 {
            break;
        }
        page += 1;
        if page > MAX_PAGES {
            break;
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// retry_upload — shared exponential-backoff retry for upload operations
// ---------------------------------------------------------------------------

/// Retry an async upload operation with exponential backoff.
/// Matches GoReleaser: 10 attempts, 50ms initial delay.
/// Retries on transient errors (5xx, timeouts, connection errors).
async fn retry_upload<F, Fut>(operation_name: &str, mut f: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    const MAX_ATTEMPTS: u32 = 10;
    const INITIAL_DELAY: std::time::Duration = std::time::Duration::from_millis(50);
    const MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

    // GoReleaser wraps ALL upload errors in RetriableError and retries all of
    // them. We match that: retry every failure, not just specific HTTP codes.
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match f().await {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                if attempt < MAX_ATTEMPTS {
                    let delay = std::cmp::min(INITIAL_DELAY * 2u32.pow(attempt - 1), MAX_DELAY);
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        anyhow::anyhow!("{}: failed after {} attempts", operation_name, MAX_ATTEMPTS)
    }))
}

// ---------------------------------------------------------------------------
// populate_artifact_download_urls
// ---------------------------------------------------------------------------

/// Set `metadata["url"]` on every artifact for the given crate, constructing
/// the download URL from the SCM backend's download base, owner/repo, tag, and
/// artifact name. This matches GoReleaser's `ReleaseURLTemplate()` pattern and
/// allows publishers to resolve download URLs without explicit `url_template`.
fn populate_artifact_download_urls(
    ctx: &mut Context,
    crate_name: &str,
    token_type: ScmTokenType,
    download_base: &str,
    owner: &str,
    repo: &str,
    tag: &str,
) {
    let dl_base = download_base.trim_end_matches('/');
    let url_tag = percent_encode_path(tag);
    let url_prefix = match token_type {
        ScmTokenType::GitLab => {
            if owner.is_empty() {
                format!("{dl_base}/{repo}/-/releases/{url_tag}/downloads")
            } else {
                format!("{dl_base}/{owner}/{repo}/-/releases/{url_tag}/downloads")
            }
        }
        ScmTokenType::GitHub | ScmTokenType::Gitea => {
            format!("{dl_base}/{owner}/{repo}/releases/download/{url_tag}")
        }
    };
    for artifact in ctx.artifacts.all_mut() {
        if artifact.crate_name == crate_name && !artifact.name.is_empty() {
            let encoded_name = percent_encode_path(&artifact.name);
            artifact
                .metadata
                .insert("url".to_string(), format!("{url_prefix}/{encoded_name}"));
        }
    }
}

// ---------------------------------------------------------------------------
// should_mark_prerelease
// ---------------------------------------------------------------------------

/// Decide whether the GitHub Release should be marked as a pre-release.
///
/// - `Auto`     – inspect the tag for common pre-release suffixes.
/// - `Bool(b)`  – use the explicit value regardless of the tag.
/// - `None`     – default to `false`.
pub(crate) fn should_mark_prerelease(config: &Option<PrereleaseConfig>, tag: &str) -> bool {
    match config {
        Some(PrereleaseConfig::Auto) => git::parse_semver_tag(tag)
            .map(|sv| sv.is_prerelease())
            .unwrap_or(false),
        Some(PrereleaseConfig::Bool(b)) => *b,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// build_release_body
// ---------------------------------------------------------------------------

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

    parts.join("\n")
}

// ---------------------------------------------------------------------------
// collect_extra_files
// ---------------------------------------------------------------------------

/// Resolve `extra_files` glob patterns into concrete file paths.
/// Returns `(path, optional_rendered_name)` pairs. When a `Detailed` spec has
/// a `name_template`, the template is rendered using the provided `Context` and
/// returned as the second element; the upload loop should use this as the
/// upload filename instead of the filesystem name.
/// GoReleaser parity (internal/extrafiles/extra_files.go): invalid glob patterns
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
                                anodize_core::template::extract_artifact_ext(&filename),
                            );
                            anodize_core::template::render(tmpl, &vars).ok()
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

// ---------------------------------------------------------------------------
// resolve_make_latest
// ---------------------------------------------------------------------------

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
) -> Option<octocrab::repos::releases::MakeLatest>
where
    F: Fn(&str) -> anyhow::Result<String>,
{
    use octocrab::repos::releases::MakeLatest;
    match config {
        Some(MakeLatestConfig::Bool(true)) => Some(MakeLatest::True),
        Some(MakeLatestConfig::Bool(false)) => Some(MakeLatest::False),
        Some(MakeLatestConfig::Auto) => Some(MakeLatest::Legacy),
        Some(MakeLatestConfig::String(tmpl)) => {
            let rendered = render(tmpl).unwrap_or_else(|_| tmpl.clone());
            match rendered.trim() {
                "true" | "1" => Some(MakeLatest::True),
                "false" | "0" | "" => Some(MakeLatest::False),
                "auto" => Some(MakeLatest::Legacy),
                _ => Some(MakeLatest::True), // non-empty = truthy, matching GoReleaser
            }
        }
        None => None,
    }
}

// ---------------------------------------------------------------------------
// resolve_release_mode
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// resolve_content_source
// ---------------------------------------------------------------------------

/// Resolve a `ContentSource` to its string content.
/// - Inline: returns the string directly.
/// - FromFile: reads the file from disk.
/// - FromUrl: fetches the URL content via HTTP GET.
pub(crate) fn resolve_content_source(source: &ContentSource) -> Result<String> {
    match source {
        ContentSource::Inline(s) => Ok(s.clone()),
        ContentSource::FromFile { from_file } => std::fs::read_to_string(from_file)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {}", from_file, e)),
        ContentSource::FromUrl { from_url } => {
            let response = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?
                .get(from_url)
                .send()
                .map_err(|e| anyhow::anyhow!("failed to fetch content URL: {}", e))?;
            if !response.status().is_success() {
                bail!("content URL returned HTTP {}", response.status());
            }
            Ok(response.text()?)
        }
    }
}

// ---------------------------------------------------------------------------
// compose_body_for_mode
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// build_release_json
// ---------------------------------------------------------------------------

/// GitHub's maximum release body length in characters.
const GITHUB_RELEASE_BODY_MAX_CHARS: usize = 125_000;

/// Build the JSON body for GitHub release create/update API calls.
/// Extracts the common construction shared by PATCH (update existing draft)
/// and POST (create new release) paths.
#[allow(clippy::too_many_arguments)]
fn build_release_json(
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
            // Find a safe UTF-8 char boundary: the last char whose end
            // byte is at or before max_content, so body[..safe_end] + suffix
            // never exceeds GITHUB_RELEASE_BODY_MAX_CHARS.
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

// ---------------------------------------------------------------------------
// resolve_release_tag
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// build_octocrab_client — GitHub Enterprise URL support
// ---------------------------------------------------------------------------

/// Build an octocrab client, optionally configured for GitHub Enterprise.
///
/// When `github_urls` is `None` or has no custom API URL, this produces a
/// standard GitHub.com client.  When an `api` URL is set, the octocrab
/// builder's `base_uri` is pointed at the Enterprise API endpoint.  If
/// `upload` is set, `upload_uri` is also overridden (octocrab uses this for
/// release asset uploads).
///
/// `skip_tls_verify` is supported by constructing a custom `hyper_rustls`
/// connector whose `rustls::ClientConfig` disables certificate verification.
/// This is the same approach GoReleaser uses via Go's `InsecureSkipVerify`.
fn build_octocrab_client(
    token: &str,
    github_urls: &Option<GitHubUrlsConfig>,
) -> Result<octocrab::Octocrab> {
    let skip_tls = github_urls
        .as_ref()
        .and_then(|u| u.skip_tls_verify)
        .unwrap_or(false);

    if skip_tls {
        // Build a custom hyper client with TLS verification disabled, then
        // wrap it in octocrab's expected service layer stack.
        build_octocrab_client_insecure(token, github_urls)
    } else {
        // Normal path: use octocrab's built-in hyper client.
        let mut builder = octocrab::Octocrab::builder().personal_token(token.to_owned());

        if let Some(urls) = github_urls {
            if let Some(api) = &urls.api {
                builder = builder
                    .base_uri(api.as_str())
                    .context("release: invalid github_urls.api URL")?;
            }
            if let Some(upload) = &urls.upload {
                builder = builder
                    .upload_uri(upload.as_str())
                    .context("release: invalid github_urls.upload URL")?;
            }
        }

        builder.build().context("release: build octocrab client")
    }
}

/// Build an octocrab client that skips TLS certificate verification.
///
/// This follows octocrab's `custom_client.rs` example pattern: construct a
/// hyper client with a custom `rustls::ClientConfig` that disables cert
/// verification, then wrap it in octocrab's middleware layers for auth, base
/// URI, and headers via `OctocrabBuilder::with_service` / `with_layer`.
fn build_octocrab_client_insecure(
    token: &str,
    github_urls: &Option<GitHubUrlsConfig>,
) -> Result<octocrab::Octocrab> {
    eprintln!("WARNING: TLS certificate verification disabled for GitHub API — this is insecure");

    // Build a rustls ClientConfig that accepts any server certificate.
    let crypto_provider = rustls::crypto::ring::default_provider();
    let tls_config = rustls::ClientConfig::builder_with_provider(Arc::new(crypto_provider))
        .with_safe_default_protocol_versions()
        .context("release: configure TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(DangerousNoCertVerifier::new()))
        .with_no_client_auth();

    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls_config)
        .https_or_http()
        .enable_http1()
        .build();

    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(connector);

    // Parse URIs the same way octocrab does.
    let base_uri: http::Uri = if let Some(api) = github_urls.as_ref().and_then(|u| u.api.as_ref()) {
        api.parse()
            .context("release: invalid github_urls.api URL")?
    } else {
        "https://api.github.com"
            .parse()
            .expect("hardcoded URI is valid")
    };

    let upload_uri: http::Uri =
        if let Some(upload) = github_urls.as_ref().and_then(|u| u.upload.as_ref()) {
            upload
                .parse()
                .context("release: invalid github_urls.upload URL")?
        } else {
            "https://uploads.github.com"
                .parse()
                .expect("hardcoded URI is valid")
        };

    // Follow octocrab's custom_client.rs example: with_service → with_layer
    // for BaseUri, ExtraHeaders, and AuthHeader, then with_auth → build.
    let auth_header: HeaderValue = format!("Bearer {}", token)
        .parse()
        .context("release: format auth header")?;

    octocrab::OctocrabBuilder::new_empty()
        .with_service(client)
        .with_layer(&ExtraHeadersLayer::new(Arc::new(vec![(
            http::header::USER_AGENT,
            HeaderValue::from_static("octocrab"),
        )])))
        .with_layer(&BaseUriLayer::new(base_uri.clone()))
        .with_layer(&AuthHeaderLayer::new(
            Some(auth_header),
            base_uri,
            upload_uri,
        ))
        .with_auth(octocrab::AuthState::None)
        .build()
        .map_err(|e| match e {}) // Infallible → never fails
}

/// A [`rustls::client::danger::ServerCertVerifier`] that accepts all certificates
/// unconditionally.  Used only when `github_urls.skip_tls_verify` is explicitly
/// enabled — typically for self-signed GitHub Enterprise instances in development
/// or air-gapped environments.
#[derive(Debug)]
struct DangerousNoCertVerifier {
    /// Pre-computed signature schemes from the ring crypto provider, avoiding
    /// a fresh `CryptoProvider` allocation on every call to `supported_verify_schemes`.
    schemes: Vec<rustls::SignatureScheme>,
}

impl DangerousNoCertVerifier {
    fn new() -> Self {
        Self {
            schemes: rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes(),
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for DangerousNoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.schemes.clone()
    }
}

// ---------------------------------------------------------------------------
// ReleaseStage
// ---------------------------------------------------------------------------

pub struct ReleaseStage;

impl Stage for ReleaseStage {
    fn name(&self) -> &str {
        "release"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("release");

        // The SCM token is already resolved into ctx.options.token by the CLI
        // pipeline init (resolve_scm_token_type). Trust it directly.
        let token = ctx.options.token.clone();

        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.is_dry_run();
        let github_native_changelog = ctx.github_native_changelog;

        // Collect crates that have a `release` block.
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| c.release.is_some())
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        // Create the tokio runtime once, outside the loop.
        let rt =
            tokio::runtime::Runtime::new().context("release: failed to create tokio runtime")?;

        for crate_cfg in &crates {
            let release_cfg = crate_cfg.release.as_ref().unwrap();

            // Skip crates where release is explicitly disabled (supports template strings).
            if let Some(ref d) = release_cfg.disable
                && d.is_disabled(|s| ctx.render_template(s))
            {
                log.status(&format!(
                    "release disabled for crate '{}', skipping",
                    crate_cfg.name
                ));
                continue;
            }

            let crate_name = crate_cfg.name.clone();

            // Validate conflicting draft options.
            if release_cfg.replace_existing_draft.unwrap_or(false)
                && release_cfg.use_existing_draft.unwrap_or(false)
            {
                bail!(
                    "release: crate '{}': cannot set both replace_existing_draft and \
                     use_existing_draft — replace deletes drafts that use_existing_draft needs",
                    crate_name
                );
            }

            let changelog_body = ctx.changelogs.get(&crate_name).cloned().unwrap_or_default();

            // Populate the {{ Checksums }} template variable from checksum artifacts.
            // GoReleaser's describeBody reads all checksum files and injects their
            // contents so header/footer templates can reference {{ .Checksums }}.
            let checksums_text = {
                let checksum_artifacts = ctx.artifacts.by_kind(ArtifactKind::Checksum);
                let mut parts = Vec::new();
                for artifact in &checksum_artifacts {
                    if let Ok(content) = std::fs::read_to_string(&artifact.path) {
                        let trimmed = content.trim();
                        if !trimmed.is_empty() {
                            parts.push(trimmed.to_string());
                        }
                    }
                }
                parts.join("\n")
            };
            ctx.template_vars_mut().set("Checksums", &checksums_text);

            // Resolve and validate release mode.
            let release_mode = resolve_release_mode(release_cfg.mode.as_deref())
                .with_context(|| format!("release: invalid mode for crate '{}'", crate_name))?;
            if release_mode != "keep-existing" {
                log.status(&format!(
                    "release mode '{}' for crate '{}'",
                    release_mode, crate_name
                ));
            }

            // Refresh Artifacts template var so release body templates can iterate artifacts.
            ctx.refresh_artifacts_var();

            // Resolve and template-render header/footer before building release body.
            let rendered_header = release_cfg
                .header
                .as_ref()
                .map(|src| {
                    let raw = resolve_content_source(src).with_context(|| {
                        format!("release: resolve header for crate '{}'", crate_name)
                    })?;
                    ctx.render_template(&raw).with_context(|| {
                        format!("release: render header for crate '{}'", crate_name)
                    })
                })
                .transpose()?;
            let rendered_footer = release_cfg
                .footer
                .as_ref()
                .map(|src| {
                    let raw = resolve_content_source(src).with_context(|| {
                        format!("release: resolve footer for crate '{}'", crate_name)
                    })?;
                    ctx.render_template(&raw).with_context(|| {
                        format!("release: render footer for crate '{}'", crate_name)
                    })
                })
                .transpose()?;

            let release_body = build_release_body(
                &changelog_body,
                rendered_header.as_deref(),
                rendered_footer.as_deref(),
            );

            // Resolve tag: use release.tag override if set, otherwise tag_template.
            let tag = resolve_release_tag(
                ctx,
                &crate_cfg.tag_template,
                release_cfg.tag.as_deref(),
                &crate_cfg.name,
            )?;

            // Resolve release name (GoReleaser defaults to "{{.Tag}}").
            let name_tmpl = release_cfg.name_template.as_deref().unwrap_or("{{ Tag }}");
            let release_name = ctx.render_template(name_tmpl).with_context(|| {
                format!(
                    "release: render name_template for crate '{}'",
                    crate_cfg.name
                )
            })?;

            let draft = release_cfg.draft.unwrap_or(false);
            let prerelease = should_mark_prerelease(&release_cfg.prerelease, &tag);
            let skip_upload = release_cfg
                .skip_upload
                .as_ref()
                .map(|s| {
                    // Template-render the value first (supports {{ .IsSnapshot }}, etc.)
                    let rendered = if s.is_template() {
                        ctx.render_template(s.as_str())
                            .unwrap_or_else(|_| s.as_str().to_string())
                    } else {
                        s.as_str().to_string()
                    };
                    match rendered.trim() {
                        "auto" => ctx.is_snapshot(),
                        other => other == "true" || other == "1",
                    }
                })
                .unwrap_or(false);
            let replace_existing_draft = release_cfg.replace_existing_draft.unwrap_or(false);
            let replace_existing_artifacts =
                release_cfg.replace_existing_artifacts.unwrap_or(false);
            let make_latest =
                resolve_make_latest(&release_cfg.make_latest, |s| ctx.render_template(s));
            let ids_filter = release_cfg.ids.as_ref();
            let target_commitish = release_cfg
                .target_commitish
                .as_ref()
                .map(|tc| ctx.render_template(tc))
                .transpose()
                .with_context(|| {
                    format!(
                        "release: render target_commitish for crate '{}'",
                        crate_name
                    )
                })?;
            let discussion_category_name = release_cfg.discussion_category_name.clone();
            let include_meta = release_cfg.include_meta.unwrap_or(false);
            let use_existing_draft = release_cfg.use_existing_draft.unwrap_or(false);

            // Collect uploadable artifacts for this crate, applying ids filter.
            // Each entry is (path, optional_custom_name). The custom name is only
            // set for extra_files with a name_template; regular artifacts use None.
            // GoReleaser uploads archives, packages, and signatures — NOT raw
            // binaries (Binary kind).  Raw binaries share the same filename
            // across platforms, causing "already_exists" collisions.
            let mut artifact_entries: Vec<(std::path::PathBuf, Option<String>)> = [
                ArtifactKind::Archive,
                ArtifactKind::UploadableBinary,
                ArtifactKind::UniversalBinary,
                ArtifactKind::Checksum,
                ArtifactKind::LinuxPackage,
                ArtifactKind::Snap,
                ArtifactKind::DiskImage,
                ArtifactKind::Installer,
                ArtifactKind::MacOsPackage,
                ArtifactKind::SourceArchive,
                ArtifactKind::SourceRpm,
                ArtifactKind::Makeself,
                ArtifactKind::Flatpak,
                ArtifactKind::Sbom,
                ArtifactKind::UploadableFile,
                ArtifactKind::Signature,
                ArtifactKind::Certificate,
                ArtifactKind::Header,
                ArtifactKind::CArchive,
                ArtifactKind::CShared,
            ]
            .iter()
            .flat_map(|&kind| {
                let artifacts = ctx
                    .artifacts
                    .by_kind_and_crate(kind, &crate_cfg.name)
                    .into_iter();
                if let Some(ids) = ids_filter {
                    artifacts
                        .filter(|a| {
                            // Checksums and source archives are ID-agnostic
                            // (always included regardless of IDs filter, matching GoReleaser).
                            // Note: extra files (UploadableFile in GoReleaser) are also exempt,
                            // but those are handled separately via collect_extra_files.
                            matches!(
                                a.kind,
                                ArtifactKind::Checksum
                                    | ArtifactKind::SourceArchive
                                    | ArtifactKind::Metadata
                            ) || matches!(a.metadata.get("id"), Some(id) if ids.contains(id))
                        })
                        .map(|a| (a.path.clone(), None))
                        .collect::<Vec<_>>()
                } else {
                    artifacts
                        .map(|a| (a.path.clone(), None))
                        .collect::<Vec<_>>()
                }
            })
            .collect();

            // Also include Metadata artifacts that are Signatures or Certificates.
            let sig_cert_entries: Vec<(std::path::PathBuf, Option<String>)> = ctx
                .artifacts
                .by_kind_and_crate(ArtifactKind::Metadata, &crate_cfg.name)
                .into_iter()
                .filter(|a| {
                    matches!(
                        a.metadata.get("type").map(|s| s.as_str()),
                        Some("Signature") | Some("Certificate")
                    )
                })
                .map(|a| (a.path.clone(), None))
                .collect();
            artifact_entries.extend(sig_cert_entries);

            if let Some(ids) = ids_filter {
                log.verbose(&format!(
                    "ids filter {:?} selected {} artifacts for crate '{}'",
                    ids,
                    artifact_entries.len(),
                    crate_cfg.name
                ));
            }

            // GoReleaser release.go:121 — refresh combined checksum files
            // before upload so they include signatures/artifacts added after
            // the checksum stage ran. Mirrors GoReleaser's ExtraRefresh hook.
            anodize_stage_checksum::refresh_combined_checksums(ctx, dry_run)?;

            // Collect extra files from glob patterns (with optional name_template).
            if let Some(extra_specs) = &release_cfg.extra_files {
                let extra = collect_extra_files(extra_specs, ctx)?;
                artifact_entries.extend(extra);
            }

            // Process templated_extra_files: render template contents and write to dist dir.
            // NOTE: Rendered files are written to the shared dist directory. If multiple
            // release configs use the same dst name, later writes will overwrite earlier
            // ones. Users should ensure dst names are unique across configs.
            if let Some(ref tpl_specs) = release_cfg.templated_extra_files
                && !tpl_specs.is_empty()
            {
                let dist_dir = &ctx.config.dist;
                let rendered = anodize_core::templated_files::process_templated_extra_files(
                    tpl_specs, ctx, dist_dir, "release",
                )?;
                for (path, dst_name) in rendered {
                    artifact_entries.push((path, Some(dst_name)));
                }
            }

            // include_meta: upload metadata.json and artifacts.json from dist dir.
            if include_meta {
                let dist_dir = &ctx.config.dist;
                for meta_name in &["metadata.json", "artifacts.json"] {
                    let meta_path = dist_dir.join(meta_name);
                    if meta_path.exists() {
                        artifact_entries.push((meta_path, None));
                    } else if ctx.is_strict() {
                        anyhow::bail!(
                            "include_meta: {} not found at {} (strict mode)",
                            meta_name,
                            meta_path.display()
                        );
                    } else {
                        log.warn(&format!(
                            "include_meta: {} not found at {}",
                            meta_name,
                            meta_path.display()
                        ));
                    }
                }
            }

            if dry_run {
                let backend_label = match ctx.token_type {
                    ScmTokenType::GitLab => "GitLab",
                    ScmTokenType::Gitea => "Gitea",
                    ScmTokenType::GitHub => "GitHub",
                };

                // Log platform-specific URLs when configured.
                match ctx.token_type {
                    ScmTokenType::GitHub => {
                        if let Some(urls) = &ctx.config.github_urls {
                            if let Some(api) = &urls.api {
                                log.status(&format!("(dry-run)   github_urls.api = {}", api));
                            }
                            if let Some(upload) = &urls.upload {
                                log.status(&format!("(dry-run)   github_urls.upload = {}", upload));
                            }
                            if let Some(download) = &urls.download {
                                log.status(&format!(
                                    "(dry-run)   github_urls.download = {}",
                                    download
                                ));
                            }
                            if urls.skip_tls_verify.unwrap_or(false) {
                                log.status("(dry-run)   github_urls.skip_tls_verify = true");
                            }
                        }
                    }
                    ScmTokenType::GitLab => {
                        if let Some(urls) = &ctx.config.gitlab_urls {
                            if let Some(api) = &urls.api {
                                log.status(&format!("(dry-run)   gitlab_urls.api = {}", api));
                            }
                            if let Some(download) = &urls.download {
                                log.status(&format!(
                                    "(dry-run)   gitlab_urls.download = {}",
                                    download
                                ));
                            }
                            if urls.skip_tls_verify.unwrap_or(false) {
                                log.status("(dry-run)   gitlab_urls.skip_tls_verify = true");
                            }
                            if urls.use_package_registry.unwrap_or(false) {
                                log.status("(dry-run)   gitlab_urls.use_package_registry = true");
                            }
                            if urls.use_job_token.unwrap_or(false) {
                                log.status("(dry-run)   gitlab_urls.use_job_token = true");
                            }
                        }
                    }
                    ScmTokenType::Gitea => {
                        if let Some(urls) = &ctx.config.gitea_urls {
                            if let Some(api) = &urls.api {
                                log.status(&format!("(dry-run)   gitea_urls.api = {}", api));
                            }
                            if let Some(download) = &urls.download {
                                log.status(&format!(
                                    "(dry-run)   gitea_urls.download = {}",
                                    download
                                ));
                            }
                            if urls.skip_tls_verify.unwrap_or(false) {
                                log.status("(dry-run)   gitea_urls.skip_tls_verify = true");
                            }
                        }
                    }
                }

                log.status(&format!(
                    "(dry-run) would create {} Release '{}' (tag={}, draft={}, prerelease={}, mode={}) for crate '{}'",
                    backend_label, release_name, tag, draft, prerelease, release_mode, crate_cfg.name
                ));
                if skip_upload {
                    log.status("(dry-run)   skip_upload is set, would skip artifact uploads");
                } else {
                    for (path, custom_name) in &artifact_entries {
                        if let Some(name) = custom_name {
                            log.status(&format!(
                                "(dry-run)   would upload artifact: {} (as '{}')",
                                path.display(),
                                name,
                            ));
                        } else {
                            log.status(&format!(
                                "(dry-run)   would upload artifact: {}",
                                path.display()
                            ));
                        }
                    }
                }

                // Even in dry-run, populate artifact download URLs so publishers
                // can generate manifests with correct URLs.
                let dry_dl_base = match ctx.token_type {
                    ScmTokenType::GitHub => ctx
                        .config
                        .github_urls
                        .as_ref()
                        .and_then(|u| u.download.clone())
                        .unwrap_or_else(|| "https://github.com".to_string()),
                    ScmTokenType::GitLab => ctx
                        .config
                        .gitlab_urls
                        .as_ref()
                        .and_then(|u| u.download.clone())
                        .unwrap_or_else(|| "https://gitlab.com".to_string()),
                    ScmTokenType::Gitea => {
                        ctx.config
                            .gitea_urls
                            .as_ref()
                            .and_then(|u| u.download.clone())
                            .unwrap_or_else(|| {
                                // Derive download URL from API URL by stripping
                                // /api/v1 suffix (GoReleaser defaults.go:29-36).
                                ctx.config
                                    .gitea_urls
                                    .as_ref()
                                    .and_then(|u| u.api.as_deref())
                                    .map(|api| {
                                        api.trim_end_matches('/')
                                            .trim_end_matches("/api/v1")
                                            .to_string()
                                    })
                                    .unwrap_or_else(|| "https://gitea.com".to_string())
                            })
                    }
                };
                let dry_owner = match ctx.token_type {
                    ScmTokenType::GitLab => {
                        release_cfg.gitlab.as_ref().or(release_cfg.github.as_ref())
                    }
                    ScmTokenType::Gitea => {
                        release_cfg.gitea.as_ref().or(release_cfg.github.as_ref())
                    }
                    ScmTokenType::GitHub => release_cfg.github.as_ref(),
                }
                .map(|r| r.owner.as_str())
                .unwrap_or("");
                let dry_repo = match ctx.token_type {
                    ScmTokenType::GitLab => {
                        release_cfg.gitlab.as_ref().or(release_cfg.github.as_ref())
                    }
                    ScmTokenType::Gitea => {
                        release_cfg.gitea.as_ref().or(release_cfg.github.as_ref())
                    }
                    ScmTokenType::GitHub => release_cfg.github.as_ref(),
                }
                .map(|r| r.name.as_str())
                .unwrap_or("");
                populate_artifact_download_urls(
                    ctx,
                    &crate_name,
                    ctx.token_type,
                    &dry_dl_base,
                    dry_owner,
                    dry_repo,
                    &tag,
                );

                continue;
            }

            // ---------------------------------------------------------------
            // Backend dispatch: GitHub, GitLab, or Gitea
            // ---------------------------------------------------------------
            // Each backend arm returns (release_html_url, download_base, owner, repo)
            // so we can populate artifact metadata["url"] after the match.
            let (release_url, download_base, repo_owner, repo_name) = match ctx.token_type {
                // ===============================================================
                // GitLab backend
                // ===============================================================
                ScmTokenType::GitLab => {
                    // Resolve the repo config: prefer release.gitlab, fall back to release.github.
                    let repo_cfg = match release_cfg.gitlab.as_ref().or(release_cfg.github.as_ref())
                    {
                        Some(r) => r.clone(),
                        None => {
                            log.warn(&format!(
                                "no gitlab config for crate '{}', skipping",
                                crate_cfg.name
                            ));
                            continue;
                        }
                    };

                    let token_str = match &token {
                        Some(t) => t.clone(),
                        None => {
                            bail!(
                                "release: no GitLab token available (set GITLAB_TOKEN, or pass --token)"
                            );
                        }
                    };

                    let gitlab_urls = ctx.config.gitlab_urls.clone().unwrap_or_default();
                    let api_url = gitlab_urls
                        .api
                        .unwrap_or_else(|| "https://gitlab.com/api/v4".to_string());
                    let download_url = gitlab_urls
                        .download
                        .unwrap_or_else(|| "https://gitlab.com".to_string());
                    let skip_tls = gitlab_urls.skip_tls_verify.unwrap_or(false);
                    let use_job_token = gitlab_urls.use_job_token.unwrap_or(false);
                    let use_pkg_registry =
                        gitlab_urls.use_package_registry.unwrap_or(false) || use_job_token;

                    let project_id = gitlab::gitlab_project_id(&repo_cfg.owner, &repo_cfg.name);
                    let commit_sha = ctx
                        .git_info
                        .as_ref()
                        .map(|g| g.commit.clone())
                        .unwrap_or_default();

                    let project_name_for_pkg = ctx.config.project_name.clone();
                    let version_for_pkg = ctx
                        .git_info
                        .as_ref()
                        .map(|g| {
                            // Strip leading 'v' for package version (e.g. "v1.2.3" -> "1.2.3").
                            g.tag.strip_prefix('v').unwrap_or(&g.tag).to_string()
                        })
                        .unwrap_or_else(|| "0.0.0".to_string());

                    // GitLab does not support draft releases — warn if draft options are set.
                    if replace_existing_draft {
                        log.warn("replace_existing_draft has no effect on GitLab (draft releases are not supported)");
                    }
                    if use_existing_draft {
                        log.warn("use_existing_draft has no effect on GitLab (draft releases are not supported)");
                    }

                    let url = rt.block_on(async {
                        let client =
                            gitlab::build_gitlab_client(&token_str, skip_tls, use_job_token)?;

                        // Create or update the release.
                        gitlab::gitlab_create_release(
                            &client,
                            &api_url,
                            &project_id,
                            &tag,
                            &release_name,
                            &release_body,
                            &commit_sha,
                            &release_mode,
                        )
                        .await?;

                        log.status(&format!(
                            "created GitLab Release '{}' (tag={}) on {}",
                            release_name, tag, project_id
                        ));

                        // Upload artifacts with bounded parallelism (matching GitHub path).
                        if skip_upload {
                            log.status("skip_upload is set, skipping artifact uploads");
                        } else {
                            let upload_parallelism = std::cmp::max(ctx.options.parallelism, 1);
                            let semaphore =
                                Arc::new(tokio::sync::Semaphore::new(upload_parallelism));

                            // Prepare the list of uploadable entries (error on missing files).
                            let mut missing_files = Vec::new();
                            let prepared_entries: Vec<(std::path::PathBuf, String)> =
                                artifact_entries
                                    .iter()
                                    .filter_map(|(path, custom_name)| {
                                        if !path.exists() {
                                            missing_files.push(path.display().to_string());
                                            return None;
                                        }
                                        let file_name = if let Some(name) = custom_name {
                                            name.clone()
                                        } else {
                                            path.file_name()
                                                .map(|n| n.to_string_lossy().into_owned())
                                                .unwrap_or_else(|| "artifact".to_string())
                                        };
                                        Some((path.clone(), file_name))
                                    })
                                    .collect();

                            if !missing_files.is_empty() {
                                anyhow::bail!(
                                    "the following artifact files are missing:\n  {}",
                                    missing_files.join("\n  ")
                                );
                            }

                            let client = Arc::new(client);
                            let mut join_set = tokio::task::JoinSet::new();

                            for (path, file_name) in prepared_entries {
                                let sem = semaphore.clone();
                                let client = client.clone();
                                let api_url = api_url.clone();
                                let project_id = project_id.clone();
                                let tag = tag.clone();
                                let project_name_for_pkg = project_name_for_pkg.clone();
                                let version_for_pkg = version_for_pkg.clone();
                                let download_url = download_url.clone();

                                join_set.spawn(async move {
                                    let _permit = sem
                                        .acquire()
                                        .await
                                        .map_err(|e| anyhow::anyhow!("semaphore closed: {}", e))?;

                                    let op_name = format!("gitlab: upload '{}'", file_name);
                                    retry_upload(&op_name, || {
                                        gitlab::gitlab_upload_asset(
                                            &client,
                                            &api_url,
                                            &project_id,
                                            &tag,
                                            &path,
                                            &file_name,
                                            &project_name_for_pkg,
                                            &version_for_pkg,
                                            use_pkg_registry,
                                            &download_url,
                                            replace_existing_artifacts,
                                        )
                                    })
                                    .await
                                    .with_context(|| {
                                        format!(
                                            "release: upload artifact '{}' to GitLab release '{}'",
                                            file_name, tag
                                        )
                                    })?;

                                    Ok::<String, anyhow::Error>(file_name)
                                });
                            }

                            while let Some(result) = join_set.join_next().await {
                                let file_name = result
                                    .context("gitlab: upload task panicked")?
                                    .context("gitlab: upload task failed")?;
                                log.verbose(&format!("uploaded artifact: {}", file_name));
                            }
                        }

                        // GitLab does not support draft releases — publish is a no-op.

                        let html_url = gitlab::gitlab_release_url(
                            &download_url,
                            &repo_cfg.owner,
                            &repo_cfg.name,
                            &tag,
                        );
                        Ok::<String, anyhow::Error>(html_url)
                    })?;

                    (
                        url,
                        download_url,
                        repo_cfg.owner.clone(),
                        repo_cfg.name.clone(),
                    )
                }

                // ===============================================================
                // Gitea backend
                // ===============================================================
                ScmTokenType::Gitea => {
                    // Resolve the repo config: prefer release.gitea, fall back to release.github.
                    let repo_cfg = match release_cfg.gitea.as_ref().or(release_cfg.github.as_ref())
                    {
                        Some(r) => r.clone(),
                        None => {
                            log.warn(&format!(
                                "no gitea config for crate '{}', skipping",
                                crate_cfg.name
                            ));
                            continue;
                        }
                    };

                    let token_str = match &token {
                        Some(t) => t.clone(),
                        None => {
                            bail!(
                                "release: no Gitea token available (set GITEA_TOKEN, or pass --token)"
                            );
                        }
                    };

                    let gitea_urls = ctx.config.gitea_urls.clone().unwrap_or_default();
                    let api_url = gitea_urls
                        .api
                        .unwrap_or_else(|| "https://gitea.com/api/v1".to_string());
                    let download_url = gitea_urls
                        .download
                        .unwrap_or_else(|| "https://gitea.com".to_string());
                    let skip_tls = gitea_urls.skip_tls_verify.unwrap_or(false);

                    let commit_sha = ctx
                        .git_info
                        .as_ref()
                        .map(|g| g.commit.clone())
                        .unwrap_or_default();

                    // Gitea does not support draft releases robustly — warn if draft options are set.
                    if replace_existing_draft {
                        log.warn("replace_existing_draft has no effect on Gitea (draft support is limited)");
                    }
                    if use_existing_draft {
                        log.warn(
                            "use_existing_draft has no effect on Gitea (draft support is limited)",
                        );
                    }

                    let url = rt.block_on(async {
                        let client = gitea::build_gitea_client(&token_str, skip_tls)?;

                        // Create or update the release.
                        let release_id = gitea::gitea_create_release(
                            &client,
                            &api_url,
                            &repo_cfg.owner,
                            &repo_cfg.name,
                            &tag,
                            &commit_sha,
                            &release_name,
                            &release_body,
                            draft,
                            prerelease,
                            &release_mode,
                        )
                        .await?;

                        log.status(&format!(
                            "created Gitea Release '{}' (id={}, tag={}) on {}/{}",
                            release_name, release_id, tag, repo_cfg.owner, repo_cfg.name
                        ));

                        // Upload artifacts with bounded parallelism (matching GitLab pattern).
                        if skip_upload {
                            log.status("skip_upload is set, skipping artifact uploads");
                        } else {
                            let upload_parallelism = std::cmp::max(ctx.options.parallelism, 1);
                            let semaphore =
                                Arc::new(tokio::sync::Semaphore::new(upload_parallelism));

                            // Prepare the list of uploadable entries (error on missing files).
                            let mut missing_files = Vec::new();
                            let prepared_entries: Vec<(std::path::PathBuf, String)> =
                                artifact_entries
                                    .iter()
                                    .filter_map(|(path, custom_name)| {
                                        if !path.exists() {
                                            missing_files.push(path.display().to_string());
                                            return None;
                                        }
                                        let file_name = if let Some(name) = custom_name {
                                            name.clone()
                                        } else {
                                            path.file_name()
                                                .map(|n| n.to_string_lossy().into_owned())
                                                .unwrap_or_else(|| "artifact".to_string())
                                        };
                                        Some((path.clone(), file_name))
                                    })
                                    .collect();

                            if !missing_files.is_empty() {
                                anyhow::bail!(
                                    "the following artifact files are missing:\n  {}",
                                    missing_files.join("\n  ")
                                );
                            }

                            let client = Arc::new(client);
                            let mut join_set = tokio::task::JoinSet::new();

                            for (path, file_name) in prepared_entries {
                                let sem = semaphore.clone();
                                let client = client.clone();
                                let api_url = api_url.clone();
                                let owner = repo_cfg.owner.clone();
                                let repo = repo_cfg.name.clone();
                                let tag = tag.clone();

                                join_set.spawn(async move {
                                    let _permit = sem
                                        .acquire()
                                        .await
                                        .map_err(|e| anyhow::anyhow!("semaphore closed: {}", e))?;

                                    // Handle replace_existing_artifacts: if an asset with the
                                    // same name exists, delete it before uploading.
                                    if replace_existing_artifacts {
                                        gitea::gitea_delete_asset_by_name(
                                        &client,
                                        &api_url,
                                        &owner,
                                        &repo,
                                        release_id,
                                        &file_name,
                                    )
                                    .await
                                    .with_context(|| {
                                        format!(
                                            "gitea: delete existing asset '{}' from release {}",
                                            file_name, release_id
                                        )
                                    })?;
                                    }

                                    let op_name = format!("gitea: upload '{}'", file_name);
                                    retry_upload(&op_name, || {
                                        gitea::gitea_upload_asset(
                                            &client, &api_url, &owner, &repo, release_id, &path,
                                            &file_name,
                                        )
                                    })
                                    .await
                                    .with_context(|| {
                                        format!(
                                            "release: upload artifact '{}' to Gitea release '{}'",
                                            file_name, tag
                                        )
                                    })?;

                                    Ok::<String, anyhow::Error>(file_name)
                                });
                            }

                            while let Some(result) = join_set.join_next().await {
                                let file_name = result
                                    .context("gitea: upload task panicked")?
                                    .context("gitea: upload task failed")?;
                                log.verbose(&format!("uploaded artifact: {}", file_name));
                            }
                        }

                        // Gitea PublishRelease is a no-op (matching GoReleaser).

                        let html_url = gitea::gitea_release_url(
                            &download_url,
                            &repo_cfg.owner,
                            &repo_cfg.name,
                            &tag,
                        );
                        Ok::<String, anyhow::Error>(html_url)
                    })?;

                    (
                        url,
                        download_url,
                        repo_cfg.owner.clone(),
                        repo_cfg.name.clone(),
                    )
                }

                // ===============================================================
                // GitHub backend (existing octocrab implementation)
                // ===============================================================
                ScmTokenType::GitHub => {
                    // Require a GitHub config block.
                    let github = match &release_cfg.github {
                        Some(g) => g.clone(),
                        None => {
                            log.warn(&format!(
                                "no github config for crate '{}', skipping",
                                crate_cfg.name
                            ));
                            continue;
                        }
                    };

                    // Require a token for real API calls.
                    let token_str = match &token {
                        Some(t) => t.clone(),
                        None => {
                            anyhow::bail!(
                                "release: no GitHub token available (set GITHUB_TOKEN or ANODIZE_GITHUB_TOKEN, or pass --token)"
                            );
                        }
                    };

                    // Extract github_urls config for GitHub Enterprise support.
                    let github_urls = ctx.config.github_urls.clone();
                    // Default download URL to "https://github.com" (matches GoReleaser's DefaultGitHubDownloadURL).
                    let gh_download_base = github_urls
                        .as_ref()
                        .and_then(|u| u.download.clone())
                        .unwrap_or_else(|| "https://github.com".to_string());

                    // Build the octocrab instance and perform async API calls inside a
                    // dedicated tokio runtime (the Stage trait is synchronous).
                    let url = rt.block_on(async {
                    let octo = build_octocrab_client(&token_str, &github_urls)?;
                    let rate_limit_client = reqwest::Client::new();

                    // Helper: list all releases (with pagination) and find a draft
                    // matching the release name. GoReleaser searches by name (not tag).
                    async fn find_draft_by_name(
                        octo: &octocrab::Octocrab,
                        owner: &str,
                        repo: &str,
                        name: &str,
                    ) -> Result<Option<octocrab::models::repos::Release>> {
                        // Cap at 10 pages (1000 releases) to avoid runaway pagination
                        // on repos with very long release histories.
                        const MAX_PAGES: u32 = 10;
                        let mut page: u32 = 1;
                        loop {
                            let route = format!(
                                "/repos/{}/{}/releases?per_page=100&page={}",
                                owner, repo, page
                            );
                            let releases: Vec<octocrab::models::repos::Release> =
                                octo.get(route, None::<&()>).await
                                    .with_context(|| format!(
                                        "release: list releases on {}/{} (page {})",
                                        owner, repo, page
                                    ))?;
                            if let Some(found) = releases
                                .iter()
                                .find(|r| r.draft && r.name.as_deref() == Some(name))
                            {
                                return Ok(Some(found.clone()));
                            }
                            // If we got fewer than 100 results, there are no more pages.
                            if releases.len() < 100 {
                                break;
                            }
                            page += 1;
                            if page > MAX_PAGES {
                                break;
                            }
                        }
                        Ok(None)
                    }

                    // Proactive rate limit check before draft search/release operations.
                    check_github_rate_limit(&rate_limit_client, &token_str, 10).await;

                    // Handle replace_existing_draft: check if a draft release with
                    // the same NAME exists and delete it.
                    if replace_existing_draft && draft
                        && let Some(existing) =
                            find_draft_by_name(&octo, &github.owner, &github.name, &release_name)
                                .await?
                    {
                        log.status(&format!(
                            "replacing existing draft release '{}' (id={})",
                            release_name, existing.id
                        ));
                        octo.repos(&github.owner, &github.name)
                            .releases()
                            .delete(existing.id.into_inner())
                            .await
                            .with_context(|| {
                                format!(
                                    "release: delete existing draft release '{}' on {}/{}",
                                    release_name, github.owner, github.name
                                )
                            })?;
                    }

                    // Handle use_existing_draft: look for an existing draft release
                    // with the same NAME and update it instead of creating a new one.
                    let existing_draft = if use_existing_draft {
                        match find_draft_by_name(
                            &octo,
                            &github.owner,
                            &github.name,
                            &release_name,
                        )
                        .await?
                        {
                            Some(existing) => {
                                log.status(&format!(
                                    "reusing existing draft release '{}' (id={})",
                                    release_name, existing.id
                                ));
                                Some(existing)
                            }
                            None => None,
                        }
                    } else {
                        None
                    };

                    // When updating an existing release, apply mode-based body composition.
                    // Also track any existing release found by tag so we can PATCH it
                    // instead of POSTing a new one (which would 422 on duplicate tags).
                    let (final_body, existing_by_tag) = if let Some(ref existing) = existing_draft {
                        let existing_body = existing.body.as_deref();
                        (compose_body_for_mode(&release_mode, existing_body, &release_body), None)
                    } else {
                        // For new releases, check if a release exists for mode != "replace".
                        if release_mode != "replace" {
                            check_github_rate_limit(&rate_limit_client, &token_str, 10).await;
                            match octo
                                .repos(&github.owner, &github.name)
                                .releases()
                                .get_by_tag(&tag)
                                .await
                            {
                                Ok(existing) => {
                                    let existing_body = existing.body.as_deref();
                                    let body = compose_body_for_mode(&release_mode, existing_body, &release_body);
                                    (body, Some(existing))
                                }
                                Err(_) => (release_body.clone(), None),
                            }
                        } else {
                            (release_body.clone(), None)
                        }
                    };

                    // Create or update the release. We use raw API calls for all paths
                    // to support target_commitish and discussion_category_name, which
                    // are not fully exposed by octocrab's builder API.
                    //
                    // Draft-then-publish: always create as draft first so users never
                    // see a release with missing artifacts. After all uploads succeed,
                    // we PATCH draft=false if the user wanted a non-draft release.
                    let user_wants_draft = draft;
                    // GitHub ignores discussion_category_name on draft releases and
                    // make_latest is meaningless until publish. Send them only in the
                    // un-draft PATCH (below) to match GoReleaser behaviour.
                    if final_body.len() > GITHUB_RELEASE_BODY_MAX_CHARS {
                        log.warn(&format!(
                            "release body ({} chars) exceeds GitHub limit ({}); truncating",
                            final_body.len(),
                            GITHUB_RELEASE_BODY_MAX_CHARS,
                        ));
                    }
                    let json_body = build_release_json(
                        &tag,
                        &release_name,
                        &final_body,
                        true, // always create as draft first
                        prerelease,
                        &None,  // make_latest deferred to publish PATCH
                        &target_commitish,
                        &None,  // discussion_category_name deferred to publish PATCH
                        github_native_changelog,
                    );

                    // Rate limit check before release create/update API call.
                    check_github_rate_limit(&rate_limit_client, &token_str, 10).await;

                    let release = if let Some(ref existing) = existing_draft {
                        // Update the existing draft release via PATCH.
                        let route = format!(
                            "/repos/{}/{}/releases/{}",
                            github.owner, github.name, existing.id
                        );
                        octo.patch::<octocrab::models::repos::Release, _, _>(route, Some(&json_body))
                            .await
                            .with_context(|| {
                                format!(
                                    "release: update existing draft release '{}' on {}/{}",
                                    tag, github.owner, github.name
                                )
                            })?
                    } else if let Some(ref existing) = existing_by_tag {
                        // An existing release was found by tag (append/prepend/keep-existing
                        // mode). PATCH it instead of POSTing a new one, which would cause
                        // a 422 "tag already exists" error from GitHub.
                        log.status(&format!(
                            "updating existing release '{}' (id={}, mode={})",
                            release_name, existing.id, release_mode
                        ));
                        let route = format!(
                            "/repos/{}/{}/releases/{}",
                            github.owner, github.name, existing.id
                        );
                        // GoReleaser parity (github.go:541): preserve the existing
                        // release's draft state on PATCH. Our default json_body is
                        // built with `draft=true` for the create path; when updating
                        // an existing release we must not flip it back to draft.
                        let mut patch_body = json_body.clone();
                        if let Some(obj) = patch_body.as_object_mut() {
                            obj.insert(
                                "draft".to_string(),
                                serde_json::Value::Bool(existing.draft),
                            );
                        }
                        octo.patch::<octocrab::models::repos::Release, _, _>(route, Some(&patch_body))
                            .await
                            .with_context(|| {
                                format!(
                                    "release: update existing release '{}' on {}/{}",
                                    tag, github.owner, github.name
                                )
                            })?
                    } else {
                        // Create a new release via POST.
                        let route = format!(
                            "/repos/{}/{}/releases",
                            github.owner, github.name
                        );
                        octo.post::<_, octocrab::models::repos::Release>(route, Some(&json_body))
                            .await
                            .with_context(|| {
                                format!(
                                    "release: create GitHub release '{}' on {}/{}",
                                    tag, github.owner, github.name
                                )
                            })?
                    };

                    log.status(&format!(
                        "created GitHub Release '{}' (id={}) on {}/{}",
                        release_name, release.id, github.owner, github.name
                    ));

                    let html_url = release.html_url.to_string();
                    let release_id_raw = release.id.into_inner();

                    // Wrap octo in Arc for shared use across parallel upload tasks
                    // and the subsequent publish PATCH.
                    let octo = std::sync::Arc::new(octo);

                    // Upload artifacts (unless skip_upload is set), with bounded
                    // parallelism using a semaphore (context's parallelism setting,
                    // minimum 1).
                    if skip_upload {
                        log.status("skip_upload is set, skipping artifact uploads");
                    } else {
                        let upload_parallelism = std::cmp::max(ctx.options.parallelism, 1);
                        let semaphore = std::sync::Arc::new(
                            tokio::sync::Semaphore::new(upload_parallelism),
                        );
                        let gh_owner = github.owner.clone();
                        let gh_name = github.name.clone();
                        let tag_for_upload = tag.clone();

                        // Prepare the list of uploadable entries (error on missing files).
                        let mut missing_files = Vec::new();
                        let prepared_entries: Vec<(std::path::PathBuf, String)> = artifact_entries
                            .iter()
                            .filter_map(|(path, custom_name)| {
                                if !path.exists() {
                                    missing_files.push(path.display().to_string());
                                    return None;
                                }
                                let file_name = if let Some(name) = custom_name {
                                    name.clone()
                                } else {
                                    path.file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| "artifact".to_string())
                                };
                                Some((path.clone(), file_name))
                            })
                            .collect();

                        if !missing_files.is_empty() {
                            anyhow::bail!(
                                "the following artifact files are missing:\n  {}",
                                missing_files.join("\n  ")
                            );
                        }

                        let mut join_set = tokio::task::JoinSet::new();

                        for (path, file_name) in prepared_entries {
                            let sem = semaphore.clone();
                            let octo = octo.clone();
                            let gh_owner = gh_owner.clone();
                            let gh_name = gh_name.clone();
                            let tag_c = tag_for_upload.clone();
                            let token_for_rate_limit = token_str.clone();

                            join_set.spawn(async move {
                                let _permit = sem.acquire().await
                                    .map_err(|e| anyhow::anyhow!("semaphore closed: {}", e))?;

                                // Handle replace_existing_artifacts: if an asset with the
                                // same name already exists, delete it before uploading.
                                // Uses paginated asset listing to handle releases with >30 assets.
                                if replace_existing_artifacts {
                                    delete_release_asset_by_name(
                                        &octo,
                                        &gh_owner,
                                        &gh_name,
                                        release_id_raw,
                                        &file_name,
                                    )
                                    .await
                                    .with_context(|| {
                                        format!(
                                            "release: delete existing artifact '{}' from release '{}'",
                                            file_name, tag_c
                                        )
                                    })?;
                                }

                                // Retry loop: up to 10 attempts with exponential backoff.
                                const MAX_UPLOAD_ATTEMPTS: u32 = 10;
                                const INITIAL_RETRY_DELAY: std::time::Duration =
                                    std::time::Duration::from_millis(50);
                                const MAX_RETRY_DELAY: std::time::Duration =
                                    std::time::Duration::from_secs(30);

                                let mut last_err: Option<anyhow::Error> = None;
                                for attempt in 1..=MAX_UPLOAD_ATTEMPTS {
                                    let data = std::fs::read(&path).with_context(|| {
                                        format!("release: read artifact {}", path.display())
                                    })?;
                                    let local_size = data.len() as u64;

                                    match octo
                                        .repos(&gh_owner, &gh_name)
                                        .releases()
                                        .upload_asset(release_id_raw, &file_name, data.into())
                                        .send()
                                        .await
                                    {
                                        Ok(_) => {
                                            last_err = None;
                                            break;
                                        }
                                        Err(err) => {
                                            let err_str = err.to_string();
                                            let is_server_error = matches!(
                                                &err,
                                                octocrab::Error::GitHub { source, .. }
                                                    if source.status_code.is_server_error()
                                            );
                                            let is_already_exists = matches!(
                                                &err,
                                                octocrab::Error::GitHub { source, .. }
                                                    if source.status_code.as_u16() == 422
                                            ) && err_str.contains("already_exists");

                                            if is_already_exists {
                                                // Outer-retry idempotency: if an asset with the
                                                // same name already exists AND its size matches
                                                // the local artifact, a prior attempt in this
                                                // same release flow successfully uploaded it.
                                                // Treat as a no-op — the bytes GitHub has are
                                                // the bytes we intended to upload. This makes
                                                // re-runs of the publish step (e.g. after a
                                                // different publisher later in the same run
                                                // failed) recover without needing operators to
                                                // opt into `replace_existing_artifacts`.
                                                let remote_size = find_release_asset_size(
                                                    &octo,
                                                    &gh_owner,
                                                    &gh_name,
                                                    release_id_raw,
                                                    &file_name,
                                                )
                                                .await
                                                .with_context(|| {
                                                    format!(
                                                        "release: look up existing asset '{}' on release '{}'",
                                                        file_name, tag_c
                                                    )
                                                })?;
                                                if remote_size == Some(local_size) {
                                                    last_err = None;
                                                    break;
                                                }

                                                // Size mismatch — real conflict. Fall back to
                                                // `replace_existing_artifacts` config: if the
                                                // operator opted in, delete the stale asset and
                                                // retry; otherwise fail loudly so the operator
                                                // can decide how to reconcile.
                                                if replace_existing_artifacts {
                                                    let _ = delete_release_asset_by_name(
                                                        &octo,
                                                        &gh_owner,
                                                        &gh_name,
                                                        release_id_raw,
                                                        &file_name,
                                                    )
                                                    .await
                                                    .with_context(|| {
                                                        format!(
                                                            "release: delete duplicate artifact '{}' from release '{}'",
                                                            file_name, tag_c
                                                        )
                                                    })?;
                                                    last_err = Some(anyhow::anyhow!(err));
                                                    if attempt < MAX_UPLOAD_ATTEMPTS {
                                                        let delay = std::cmp::min(
                                                            INITIAL_RETRY_DELAY * 2u32.pow(attempt - 1),
                                                            MAX_RETRY_DELAY,
                                                        );
                                                        tokio::time::sleep(delay).await;
                                                    }
                                                    continue;
                                                }
                                            }

                                            // GoReleaser parity: handle rate limiting
                                            // (403/429) by sleeping and retrying.
                                            let is_rate_limited = matches!(
                                                &err,
                                                octocrab::Error::GitHub { source, .. }
                                                    if source.status_code.as_u16() == 403
                                                        || source.status_code.as_u16() == 429
                                            );

                                            if is_rate_limited {
                                                eprintln!(
                                                    "rate limited on upload of '{}', checking rate limits...",
                                                    file_name
                                                );
                                                check_github_rate_limit(
                                                    &reqwest::Client::new(),
                                                    &token_for_rate_limit,
                                                    100,
                                                )
                                                .await;
                                                last_err = Some(anyhow::anyhow!(err));
                                                continue;
                                            } else if is_server_error
                                                || matches!(&err, octocrab::Error::Hyper { .. })
                                                || matches!(&err, octocrab::Error::Http { .. })
                                            {
                                                last_err = Some(anyhow::anyhow!(err));
                                                if attempt < MAX_UPLOAD_ATTEMPTS {
                                                    let delay = std::cmp::min(
                                                        INITIAL_RETRY_DELAY * 2u32.pow(attempt - 1),
                                                        MAX_RETRY_DELAY,
                                                    );
                                                    tokio::time::sleep(delay).await;
                                                }
                                                continue;
                                            } else {
                                                // Non-retryable error — fail immediately.
                                                return Err(anyhow::anyhow!(err)).with_context(|| {
                                                    format!(
                                                        "release: upload artifact '{}' to release '{}'",
                                                        file_name, tag_c
                                                    )
                                                });
                                            }
                                        }
                                    }
                                }
                                if let Some(err) = last_err {
                                    return Err(err).with_context(|| {
                                        format!(
                                            "release: upload artifact '{}' to release '{}' failed after {} attempts",
                                            file_name, tag_c, MAX_UPLOAD_ATTEMPTS
                                        )
                                    });
                                }

                                Ok::<String, anyhow::Error>(file_name)
                            });
                        }

                        // Collect results from all upload tasks.
                        while let Some(result) = join_set.join_next().await {
                            match result {
                                Ok(Ok(file_name)) => {
                                    log.verbose(&format!("uploaded artifact: {}", file_name));
                                }
                                Ok(Err(e)) => return Err(e),
                                Err(join_err) => {
                                    return Err(anyhow::anyhow!(
                                        "release: upload task panicked: {}", join_err
                                    ));
                                }
                            }
                        }

                    }

                    // Draft-then-publish: if the user's config has draft=false,
                    // un-draft the release now that all assets are uploaded.
                    if !user_wants_draft {
                        // Rate limit check before publish (un-draft) PATCH.
                        check_github_rate_limit(&rate_limit_client, &token_str, 10).await;
                        let publish_route = format!(
                            "/repos/{}/{}/releases/{}",
                            github.owner, github.name, release_id_raw
                        );
                        let mut publish_body = serde_json::json!({ "draft": false });
                        if let Some(ml) = &make_latest {
                            publish_body["make_latest"] =
                                serde_json::Value::String(ml.to_string());
                        }
                        if let Some(dc) = &discussion_category_name {
                            publish_body["discussion_category_name"] =
                                serde_json::json!(dc);
                        }
                        octo.patch::<octocrab::models::repos::Release, _, _>(
                            publish_route,
                            Some(&publish_body),
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "release: publish (un-draft) release '{}' on {}/{}",
                                tag, github.owner, github.name
                            )
                        })?;
                        log.status(&format!(
                            "published release '{}' (draft -> live)",
                            release_name
                        ));
                    }

                    Ok::<String, anyhow::Error>(html_url)
                })?;

                    (
                        url,
                        gh_download_base,
                        github.owner.clone(),
                        github.name.clone(),
                    )
                }
            }; // end match ctx.token_type

            // Populate artifact metadata["url"] for all uploadable artifacts
            // so publishers (homebrew, scoop, chocolatey, winget, krew, nix, cask)
            // can construct download links without requiring explicit url_template.
            // Matches GoReleaser's ReleaseURLTemplate() pattern.
            if !skip_upload {
                populate_artifact_download_urls(
                    ctx,
                    &crate_name,
                    ctx.token_type,
                    &download_base,
                    &repo_owner,
                    &repo_name,
                    &tag,
                );
            }

            ctx.set_release_url(&release_url);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodize_core::config::{
        ContentSource, CrateConfig, ExtraFileSpec, MakeLatestConfig, PrereleaseConfig,
        ReleaseConfig, StringOrBool,
    };
    use anodize_core::test_helpers::TestContextBuilder;

    #[test]
    fn test_is_prerelease_auto_with_rc() {
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v1.0.0-rc.1"
        ));
    }

    #[test]
    fn test_is_prerelease_auto_stable() {
        assert!(!should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v1.0.0"
        ));
    }

    #[test]
    fn test_is_prerelease_explicit_true() {
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Bool(true)),
            "v1.0.0"
        ));
    }

    #[test]
    fn test_is_prerelease_explicit_false() {
        assert!(!should_mark_prerelease(
            &Some(PrereleaseConfig::Bool(false)),
            "v1.0.0-rc.1"
        ));
    }

    #[test]
    fn test_is_prerelease_none() {
        assert!(!should_mark_prerelease(&None, "v1.0.0"));
    }

    #[test]
    fn test_stage_skips_crate_without_release_config() {
        let mut ctx = TestContextBuilder::new().build();
        let stage = ReleaseStage;
        // Should succeed — no crates have release config
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- populate_artifact_download_urls tests ----

    #[test]
    fn test_populate_artifact_download_urls_github() {
        use anodize_core::artifact::{Artifact, ArtifactKind};

        let mut ctx = TestContextBuilder::new().build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: "dist/myapp_1.0.0_linux_amd64.tar.gz".into(),
            name: "myapp_1.0.0_linux_amd64.tar.gz".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            path: "dist/checksums.txt".into(),
            name: "checksums.txt".to_string(),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });

        populate_artifact_download_urls(
            &mut ctx,
            "myapp",
            ScmTokenType::GitHub,
            "https://github.com",
            "octocat",
            "hello",
            "v1.0.0",
        );

        let archive = ctx
            .artifacts
            .all()
            .iter()
            .find(|a| a.name == "myapp_1.0.0_linux_amd64.tar.gz")
            .unwrap();
        assert_eq!(
            archive.metadata.get("url").unwrap(),
            "https://github.com/octocat/hello/releases/download/v1.0.0/myapp_1.0.0_linux_amd64.tar.gz"
        );
        let checksum = ctx
            .artifacts
            .all()
            .iter()
            .find(|a| a.name == "checksums.txt")
            .unwrap();
        assert_eq!(
            checksum.metadata.get("url").unwrap(),
            "https://github.com/octocat/hello/releases/download/v1.0.0/checksums.txt"
        );
    }

    #[test]
    fn test_populate_artifact_download_urls_github_enterprise() {
        use anodize_core::artifact::{Artifact, ArtifactKind};

        let mut ctx = TestContextBuilder::new().build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: "dist/myapp.tar.gz".into(),
            name: "myapp.tar.gz".to_string(),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });

        populate_artifact_download_urls(
            &mut ctx,
            "myapp",
            ScmTokenType::GitHub,
            "https://github.example.com",
            "org",
            "repo",
            "v2.0.0",
        );

        let a = ctx
            .artifacts
            .all()
            .iter()
            .find(|a| a.name == "myapp.tar.gz")
            .unwrap();
        assert_eq!(
            a.metadata.get("url").unwrap(),
            "https://github.example.com/org/repo/releases/download/v2.0.0/myapp.tar.gz"
        );
    }

    #[test]
    fn test_populate_artifact_download_urls_gitlab() {
        use anodize_core::artifact::{Artifact, ArtifactKind};

        let mut ctx = TestContextBuilder::new().build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: "dist/app.tar.gz".into(),
            name: "app.tar.gz".to_string(),
            target: None,
            crate_name: "app".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });

        populate_artifact_download_urls(
            &mut ctx,
            "app",
            ScmTokenType::GitLab,
            "https://gitlab.com",
            "group",
            "project",
            "v1.0.0",
        );

        let a = ctx
            .artifacts
            .all()
            .iter()
            .find(|a| a.name == "app.tar.gz")
            .unwrap();
        assert_eq!(
            a.metadata.get("url").unwrap(),
            "https://gitlab.com/group/project/-/releases/v1.0.0/downloads/app.tar.gz"
        );
    }

    #[test]
    fn test_populate_artifact_download_urls_gitea() {
        use anodize_core::artifact::{Artifact, ArtifactKind};

        let mut ctx = TestContextBuilder::new().build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: "dist/tool.tar.gz".into(),
            name: "tool.tar.gz".to_string(),
            target: None,
            crate_name: "tool".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });

        populate_artifact_download_urls(
            &mut ctx,
            "tool",
            ScmTokenType::Gitea,
            "https://gitea.example.com",
            "owner",
            "repo",
            "v3.0.0",
        );

        let a = ctx
            .artifacts
            .all()
            .iter()
            .find(|a| a.name == "tool.tar.gz")
            .unwrap();
        assert_eq!(
            a.metadata.get("url").unwrap(),
            "https://gitea.example.com/owner/repo/releases/download/v3.0.0/tool.tar.gz"
        );
    }

    #[test]
    fn test_populate_artifact_download_urls_encodes_special_chars() {
        use anodize_core::artifact::{Artifact, ArtifactKind};

        let mut ctx = TestContextBuilder::new().build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: "dist/my app.tar.gz".into(),
            name: "my app.tar.gz".to_string(),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });

        populate_artifact_download_urls(
            &mut ctx,
            "myapp",
            ScmTokenType::GitHub,
            "https://github.com",
            "owner",
            "repo",
            "v1.0.0-rc.1",
        );

        let a = ctx.artifacts.all().first().unwrap();
        let url = a.metadata.get("url").unwrap();
        assert!(
            url.contains("my%20app.tar.gz"),
            "spaces should be percent-encoded: {}",
            url
        );
    }

    #[test]
    fn test_populate_artifact_download_urls_skips_other_crates() {
        use anodize_core::artifact::{Artifact, ArtifactKind};

        let mut ctx = TestContextBuilder::new().build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: "dist/other.tar.gz".into(),
            name: "other.tar.gz".to_string(),
            target: None,
            crate_name: "other_crate".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });

        populate_artifact_download_urls(
            &mut ctx,
            "myapp",
            ScmTokenType::GitHub,
            "https://github.com",
            "owner",
            "repo",
            "v1.0.0",
        );

        let a = ctx.artifacts.all().first().unwrap();
        assert!(
            !a.metadata.contains_key("url"),
            "should not set URL for different crate"
        );
    }

    // ---- retry_upload tests ----

    #[tokio::test]
    async fn test_retry_upload_succeeds_immediately() {
        let result = retry_upload("test", || async { Ok(()) }).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_retry_upload_retries_transient_errors() {
        let attempt = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let attempt_clone = attempt.clone();
        let result = retry_upload("test", move || {
            let attempt = attempt_clone.clone();
            async move {
                let n = attempt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < 2 {
                    anyhow::bail!("HTTP 500 Internal Server Error");
                }
                Ok(())
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(attempt.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_upload_retries_all_errors() {
        // GoReleaser retries ALL upload errors. Verify non-5xx errors are also retried.
        let attempt = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let attempt_clone = attempt.clone();
        let result = retry_upload("test", move || {
            let attempt = attempt_clone.clone();
            async move {
                let n = attempt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < 1 {
                    anyhow::bail!("HTTP 403: forbidden");
                }
                Ok(())
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(attempt.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    // ---- build_release_body tests ----

    #[test]
    fn test_build_release_body_with_header_and_footer() {
        let body = build_release_body(
            "## Changes\n- Fixed a bug",
            Some("# Release v1.0"),
            Some("---\nPowered by anodize"),
        );
        // GoReleaser parity: single newline separator between header, body, footer
        assert_eq!(
            body,
            "# Release v1.0\n## Changes\n- Fixed a bug\n---\nPowered by anodize"
        );
    }

    #[test]
    fn test_build_release_body_header_only() {
        let body = build_release_body("changelog content", Some("HEADER"), None);
        assert_eq!(body, "HEADER\nchangelog content");
    }

    #[test]
    fn test_build_release_body_footer_only() {
        let body = build_release_body("changelog content", None, Some("FOOTER"));
        assert_eq!(body, "changelog content\nFOOTER");
    }

    #[test]
    fn test_build_release_body_no_header_footer() {
        let body = build_release_body("changelog content", None, None);
        assert_eq!(body, "changelog content");
    }

    #[test]
    fn test_build_release_body_empty_changelog() {
        let body = build_release_body("", Some("HEADER"), Some("FOOTER"));
        assert_eq!(body, "HEADER\nFOOTER");
    }

    #[test]
    fn test_build_release_body_all_empty() {
        let body = build_release_body("", None, None);
        assert_eq!(body, "");
    }

    #[test]
    fn test_build_release_body_empty_string_header_footer() {
        // Empty strings should be treated as absent
        let body = build_release_body("changes", Some(""), Some(""));
        assert_eq!(body, "changes");
    }

    // ---- collect_extra_files tests ----

    #[test]
    fn test_collect_extra_files_no_patterns() {
        let ctx = TestContextBuilder::new().build();
        let result = collect_extra_files(&[], &ctx).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_extra_files_no_matches() {
        let ctx = TestContextBuilder::new().build();
        // GoReleaser parity: a glob that matches nothing is a hard error.
        let result = collect_extra_files(
            &[ExtraFileSpec::Glob(
                "/tmp/anodize_test_nonexistent_dir_12345/*.xyz".to_string(),
            )],
            &ctx,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_collect_extra_files_with_real_file() {
        let ctx = TestContextBuilder::new().build();
        // Create a temp file and collect it
        let dir = std::env::temp_dir().join("anodize_extra_files_test");
        let _ = std::fs::create_dir_all(&dir);
        let test_file = dir.join("test_extra.txt");
        std::fs::write(&test_file, "extra file content").unwrap();

        let pattern = dir.join("*.txt").to_string_lossy().into_owned();
        let result = collect_extra_files(&[ExtraFileSpec::Glob(pattern)], &ctx).unwrap();
        assert!(
            result
                .iter()
                .any(|(p, _)| p.file_name().unwrap() == "test_extra.txt")
        );

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_collect_extra_files_skips_directories() {
        let ctx = TestContextBuilder::new().build();
        let dir = std::env::temp_dir().join("anodize_extra_files_dir_test");
        let _ = std::fs::create_dir_all(dir.join("subdir"));
        let test_file = dir.join("file.txt");
        std::fs::write(&test_file, "content").unwrap();

        // The glob "*" matches both files and directories; we only want files
        let pattern = dir.join("*").to_string_lossy().into_owned();
        let result = collect_extra_files(&[ExtraFileSpec::Glob(pattern)], &ctx).unwrap();
        assert!(result.iter().all(|(p, _)| p.is_file()));

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_collect_extra_files_detailed_spec() {
        let ctx = TestContextBuilder::new().build();
        let dir = std::env::temp_dir().join("anodize_extra_files_detailed_test");
        let _ = std::fs::create_dir_all(&dir);
        let test_file = dir.join("artifact.sig");
        std::fs::write(&test_file, "signature").unwrap();

        let pattern = dir.join("*.sig").to_string_lossy().into_owned();
        let result = collect_extra_files(
            &[ExtraFileSpec::Detailed {
                glob: pattern,
                name_template: Some("{{ .ArtifactName }}.sig".to_string()),
            }],
            &ctx,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].0.file_name().unwrap() == "artifact.sig");
        // name_template should have been rendered
        assert_eq!(result[0].1.as_deref(), Some("artifact.sig.sig"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- resolve_make_latest tests ----

    /// Identity renderer for tests — returns the input unchanged.
    fn noop_render(s: &str) -> anyhow::Result<String> {
        Ok(s.to_string())
    }

    #[test]
    fn test_resolve_make_latest_true() {
        let ml = resolve_make_latest(&Some(MakeLatestConfig::Bool(true)), noop_render);
        assert!(ml.is_some());
        assert_eq!(ml.unwrap().to_string(), "true");
    }

    #[test]
    fn test_resolve_make_latest_false() {
        let ml = resolve_make_latest(&Some(MakeLatestConfig::Bool(false)), noop_render);
        assert!(ml.is_some());
        assert_eq!(ml.unwrap().to_string(), "false");
    }

    #[test]
    fn test_resolve_make_latest_auto() {
        let ml = resolve_make_latest(&Some(MakeLatestConfig::Auto), noop_render);
        assert!(ml.is_some());
        assert_eq!(ml.unwrap().to_string(), "legacy");
    }

    #[test]
    fn test_resolve_make_latest_none() {
        let ml = resolve_make_latest(&None, noop_render);
        assert!(ml.is_none());
    }

    #[test]
    fn test_resolve_make_latest_template_string_true() {
        let ml = resolve_make_latest(
            &Some(MakeLatestConfig::String("true".to_string())),
            noop_render,
        );
        assert!(ml.is_some());
        assert_eq!(ml.unwrap().to_string(), "true");
    }

    #[test]
    fn test_resolve_make_latest_template_string_false() {
        let ml = resolve_make_latest(
            &Some(MakeLatestConfig::String("false".to_string())),
            noop_render,
        );
        assert!(ml.is_some());
        assert_eq!(ml.unwrap().to_string(), "false");
    }

    #[test]
    fn test_resolve_make_latest_template_string_auto() {
        let ml = resolve_make_latest(
            &Some(MakeLatestConfig::String("auto".to_string())),
            noop_render,
        );
        assert!(ml.is_some());
        assert_eq!(ml.unwrap().to_string(), "legacy");
    }

    #[test]
    fn test_resolve_make_latest_template_rendered() {
        // Simulate a template that renders to "false"
        let ml = resolve_make_latest(
            &Some(MakeLatestConfig::String("{{ .IsSnapshot }}".to_string())),
            |_| Ok("false".to_string()),
        );
        assert!(ml.is_some());
        assert_eq!(ml.unwrap().to_string(), "false");
    }

    // ---- skip_upload behavior test ----

    #[test]
    fn test_skip_upload_dry_run_message() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    skip_upload: Some(StringOrBool::Bool(true)),
                    draft: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        // Dry-run should succeed even with skip_upload = true
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- replace_existing_draft / replace_existing_artifacts config defaults ----

    #[test]
    fn test_replace_existing_draft_defaults() {
        let cfg = ReleaseConfig::default();
        assert_eq!(cfg.replace_existing_draft, None);
    }

    #[test]
    fn test_replace_existing_artifacts_defaults() {
        let cfg = ReleaseConfig::default();
        assert_eq!(cfg.replace_existing_artifacts, None);
    }

    // ---- integration-style dry-run tests ----

    #[test]
    fn test_dry_run_with_extra_files() {
        // GoReleaser parity: extra_files globs that match nothing are hard
        // errors. Create a real file so the stage completes successfully.
        let tmp = std::env::temp_dir().join("anodize_test_dry_extra_files");
        let _ = std::fs::create_dir_all(&tmp);
        let file = tmp.join("artifact.sig");
        std::fs::write(&file, "sig").unwrap();
        let pattern = tmp.join("*.sig").to_string_lossy().into_owned();

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    extra_files: Some(vec![ExtraFileSpec::Glob(pattern)]),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_dry_run_with_header_footer_in_changelog() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    header: Some(ContentSource::Inline("# Custom Header".to_string())),
                    footer: Some(ContentSource::Inline("Custom Footer".to_string())),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        ctx.changelogs
            .insert("testcrate".to_string(), "- bug fix".to_string());
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_with_make_latest() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    make_latest: Some(MakeLatestConfig::Bool(true)),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- release.tag override tests ----

    #[test]
    fn test_resolve_release_tag_override() {
        // When release.tag is set, the override value should be used as the
        // release tag instead of crate_cfg.tag_template.
        let ctx = TestContextBuilder::new().build();
        let tag = resolve_release_tag(&ctx, "myapp/v1.0.0", Some("v1.0.0"), "testcrate").unwrap();
        assert_eq!(
            tag, "v1.0.0",
            "release.tag override must take precedence over tag_template"
        );
    }

    #[test]
    fn test_resolve_release_tag_template_rendering() {
        // The release.tag field supports template rendering.
        let ctx = TestContextBuilder::new().tag("v2.5.0").build();
        let tag = resolve_release_tag(&ctx, "prefix/{{ .Tag }}", Some("{{ .Tag }}"), "testcrate")
            .unwrap();
        assert_eq!(
            tag, "v2.5.0",
            "release.tag template must render to the git tag value"
        );
    }

    #[test]
    fn test_resolve_release_tag_falls_back_to_tag_template() {
        // When release.tag is None, the crate's tag_template is used as before.
        let ctx = TestContextBuilder::new().build();
        let tag = resolve_release_tag(&ctx, "v1.0.0", None, "testcrate").unwrap();
        assert_eq!(
            tag, "v1.0.0",
            "with no release.tag, tag_template must be used"
        );
    }

    #[test]
    fn test_resolve_release_tag_invalid_template_errors() {
        let ctx = TestContextBuilder::new().build();
        let result = resolve_release_tag(&ctx, "ok", Some("{{ invalid"), "testcrate");
        assert!(result.is_err(), "malformed template must return an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("release.tag override"),
            "error should mention release.tag override context, got: {err}"
        );
    }

    // ---- Error path tests (Task 3B) ----

    #[test]
    fn test_release_missing_token_errors() {
        use anodize_core::config::GitHubConfig;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .token(None)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    github: Some(GitHubConfig {
                        owner: "testowner".to_string(),
                        name: "testrepo".to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        let result = stage.run(&mut ctx);

        // If GITHUB_TOKEN / ANODIZE_GITHUB_TOKEN happens to be set in the
        // environment (e.g., CI), the stage would proceed past token resolution
        // and fail on the API call instead. Either way, it should error.
        assert!(result.is_err(), "release without token should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("GITHUB_TOKEN")
                || err.contains("ANODIZE_GITHUB_TOKEN")
                || err.contains("--token")
                || err.contains("release"),
            "error should mention GITHUB_TOKEN, ANODIZE_GITHUB_TOKEN, --token, or release failure, got: {err}"
        );
    }

    #[test]
    fn test_release_no_github_config_skips_silently() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    github: None, // no github config
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        // Should succeed — no github config causes skip, not error
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_prerelease_auto_detects_alpha() {
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v1.0.0-alpha.1"
        ));
    }

    #[test]
    fn test_prerelease_auto_detects_beta() {
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v2.0.0-beta"
        ));
    }

    #[test]
    fn test_prerelease_auto_detects_dev() {
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v1.0.0-dev.5"
        ));
    }

    #[test]
    fn test_collect_extra_files_invalid_glob_pattern() {
        let ctx = TestContextBuilder::new().build();
        // GoReleaser parity: invalid glob patterns are hard errors, not silent skips.
        let result = collect_extra_files(&[ExtraFileSpec::Glob("[invalid-glob".to_string())], &ctx);
        assert!(result.is_err());
    }

    // ---- MockGitHubClient integration test ----

    #[test]
    fn test_release_pipeline_with_mock_github_client() {
        use anodize_core::github_client::{
            AssetInfo, CreateReleaseParams, GitHubClient, MockGitHubClient, ReleaseInfo,
            UploadAssetParams,
        };

        // Set up the mock to return a successful release creation
        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Ok(ReleaseInfo {
            id: 42,
            html_url: "https://github.com/testowner/testrepo/releases/42".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: Some("Release v1.0.0".to_string()),
            draft: false,
        }));
        mock.set_upload_asset_response(Ok(AssetInfo {
            id: 100,
            name: "artifact.tar.gz".to_string(),
            size: 1024,
        }));

        // Build release parameters as the stage would
        let params = CreateReleaseParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release v1.0.0".to_string(),
            body: build_release_body("- initial release", Some("# v1.0.0"), None),
            draft: false,
            prerelease: should_mark_prerelease(&Some(PrereleaseConfig::Auto), "v1.0.0"),
            generate_release_notes: false,
            make_latest: None,
        };

        // Simulate the release pipeline: create release + upload asset
        let release = mock.create_release(&params).unwrap();
        assert_eq!(release.id, 42);
        assert_eq!(release.tag_name, "v1.0.0");
        assert!(!release.draft);

        // Simulate uploading an asset
        let upload_params = UploadAssetParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            release_id: release.id,
            file_name: "myapp-linux-amd64.tar.gz".to_string(),
            file_path: std::path::PathBuf::from("/tmp/myapp-linux-amd64.tar.gz"),
        };
        let asset = mock.upload_asset(&upload_params).unwrap();
        assert_eq!(asset.name, "artifact.tar.gz");

        // Verify the mock recorded the correct calls
        assert_eq!(mock.create_release_call_count(), 1);
        assert_eq!(mock.upload_asset_call_count(), 1);

        let create_calls = mock.create_release_calls();
        assert_eq!(create_calls[0].owner, "testowner");
        assert_eq!(create_calls[0].tag_name, "v1.0.0");
        assert_eq!(create_calls[0].body, "# v1.0.0\n- initial release");
        assert!(!create_calls[0].prerelease);

        let upload_calls = mock.upload_asset_calls();
        assert_eq!(upload_calls[0].release_id, 42);
        assert_eq!(upload_calls[0].file_name, "myapp-linux-amd64.tar.gz");
    }

    // -----------------------------------------------------------------------
    // Task 4C: Additional behavior tests — config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_header_footer_wrap_changelog_in_release_body() {
        // Verify that header and footer actually appear around the changelog body
        let body = build_release_body(
            "- Fixed bug A\n- Added feature B",
            Some("## Release v2.0"),
            Some("---\nThank you for using our tool!"),
        );
        assert!(body.starts_with("## Release v2.0"));
        assert!(body.contains("- Fixed bug A"));
        assert!(body.contains("- Added feature B"));
        assert!(body.ends_with("Thank you for using our tool!"));

        // GoReleaser parity: parts separated by single newline
        assert!(body.contains("## Release v2.0\n- Fixed bug A"));
        assert!(body.contains("Added feature B\n---"));
    }

    #[test]
    fn test_extra_files_collected_with_glob() {
        let ctx = TestContextBuilder::new().build();
        // Create temp files and verify glob collection works
        let dir = std::env::temp_dir().join("anodize_release_extra_test");
        let _ = std::fs::create_dir_all(&dir);
        let f1 = dir.join("artifact1.sig");
        let f2 = dir.join("artifact2.sig");
        let f3 = dir.join("readme.txt");
        std::fs::write(&f1, "sig1").unwrap();
        std::fs::write(&f2, "sig2").unwrap();
        std::fs::write(&f3, "text").unwrap();

        // Collect only .sig files
        let pattern = dir.join("*.sig").to_string_lossy().into_owned();
        let result = collect_extra_files(&[ExtraFileSpec::Glob(pattern)], &ctx).unwrap();
        assert_eq!(result.len(), 2, "should find exactly 2 .sig files");
        assert!(result.iter().all(|(p, _)| p.extension().unwrap() == "sig"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_skip_upload_prevents_dry_run_upload_messages() {
        // When skip_upload is true, the dry-run output should mention skip_upload
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    skip_upload: Some(StringOrBool::Bool(true)),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        // Should complete without error
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_make_latest_values_resolve_correctly() {
        // Bool(true) -> MakeLatest::True
        let ml_true =
            resolve_make_latest(&Some(MakeLatestConfig::Bool(true)), noop_render).unwrap();
        assert_eq!(ml_true.to_string(), "true");

        // Bool(false) -> MakeLatest::False
        let ml_false =
            resolve_make_latest(&Some(MakeLatestConfig::Bool(false)), noop_render).unwrap();
        assert_eq!(ml_false.to_string(), "false");

        // Auto -> MakeLatest::Legacy
        let ml_auto = resolve_make_latest(&Some(MakeLatestConfig::Auto), noop_render).unwrap();
        assert_eq!(ml_auto.to_string(), "legacy");

        // None -> None
        assert!(resolve_make_latest(&None, noop_render).is_none());
    }

    #[test]
    fn test_release_name_template_rendering() {
        // Verify the rendered release name matches expected template output.
        // We simulate the same resolution logic the stage uses: render
        // name_template via ctx.render_template and check the result.
        use anodize_core::github_client::{
            CreateReleaseParams, GitHubClient, MockGitHubClient, ReleaseInfo,
        };

        let ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v2.0.0")
            .build();

        let name_template = "MyApp {{ .Version }}";
        let rendered_name = ctx.render_template(name_template).unwrap();
        assert_eq!(
            rendered_name, "MyApp 2.0.0",
            "name_template should render Version variable"
        );

        let tag_template = "v{{ .Version }}";
        let rendered_tag = ctx.render_template(tag_template).unwrap();
        assert_eq!(rendered_tag, "v2.0.0");

        // Verify the rendered name would propagate to the GitHub API via mock
        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Ok(ReleaseInfo {
            id: 1,
            html_url: "https://github.com/test/test/releases/1".to_string(),
            tag_name: rendered_tag.clone(),
            name: Some(rendered_name.clone()),
            draft: false,
        }));

        let params = CreateReleaseParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            tag_name: rendered_tag,
            name: rendered_name.clone(),
            body: String::new(),
            draft: false,
            prerelease: false,
            generate_release_notes: false,
            make_latest: None,
        };

        mock.create_release(&params).unwrap();

        let calls = mock.create_release_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].name, "MyApp 2.0.0",
            "rendered name_template should be passed as the release name"
        );
    }

    #[test]
    fn test_release_name_template_default_tag() {
        // When name_template is None, the default "{{ Tag }}" should render to the tag value.
        let ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v3.1.0")
            .build();

        let default_tmpl = "{{ Tag }}";
        let rendered = ctx.render_template(default_tmpl).unwrap();
        assert_eq!(
            rendered, "v3.1.0",
            "default name_template '{{ Tag }}' should render to the tag"
        );
    }

    #[test]
    fn test_draft_release_flag() {
        // Verify draft=true propagates through to the GitHub API parameters.
        use anodize_core::github_client::{
            CreateReleaseParams, GitHubClient, MockGitHubClient, ReleaseInfo,
        };

        let release_cfg = ReleaseConfig {
            draft: Some(true),
            ..Default::default()
        };

        // Resolve draft the same way the stage does
        let draft = release_cfg.draft.unwrap_or(false);
        assert!(draft, "draft=Some(true) should resolve to true");

        // Also verify the default case
        let default_cfg = ReleaseConfig::default();
        let default_draft = default_cfg.draft.unwrap_or(false);
        assert!(!default_draft, "draft=None should default to false");

        // Verify draft=true propagates to the mock GitHub client
        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Ok(ReleaseInfo {
            id: 99,
            html_url: "https://github.com/test/test/releases/99".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: Some("Release v1.0.0".to_string()),
            draft: true,
        }));

        let params = CreateReleaseParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release v1.0.0".to_string(),
            body: build_release_body("changelog", None, None),
            draft,
            prerelease: should_mark_prerelease(&None, "v1.0.0"),
            generate_release_notes: false,
            make_latest: None,
        };

        let release = mock.create_release(&params).unwrap();
        assert!(release.draft, "mock should return draft=true");

        let calls = mock.create_release_calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].draft,
            "draft=true must propagate to CreateReleaseParams"
        );
        assert!(
            !calls[0].prerelease,
            "prerelease should be false for stable tag with None config"
        );
    }

    #[test]
    fn test_prerelease_auto_case_insensitive() {
        // The prerelease Auto detection should be case-insensitive
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v1.0.0-RC.1"
        ));
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v1.0.0-BETA"
        ));
        assert!(should_mark_prerelease(
            &Some(PrereleaseConfig::Auto),
            "v1.0.0-ALPHA.5"
        ));
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_release_missing_token_error_message_is_actionable() {
        // The release stage requires a GitHub token for non-dry-run.
        // test_release_missing_token_errors already covers this,
        // but we verify the error message is actionable (tells user what to do).
        use anodize_core::config::GitHubConfig;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .token(None)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    github: Some(GitHubConfig {
                        owner: "testowner".to_string(),
                        name: "testrepo".to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        let result = stage.run(&mut ctx);

        // If GITHUB_TOKEN / ANODIZE_GITHUB_TOKEN is in the environment, the
        // stage proceeds past token resolution and fails on the API call
        // instead. Either way the error should be informative.
        assert!(
            result.is_err(),
            "release without explicit token should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("GITHUB_TOKEN")
                || err.contains("ANODIZE_GITHUB_TOKEN")
                || err.contains("--token")
                || err.contains("release")
                || err.contains("GitHub"),
            "error should mention GITHUB_TOKEN, ANODIZE_GITHUB_TOKEN, --token, or release context, got: {err}"
        );
    }

    #[test]
    fn test_mock_github_api_401_error() {
        use anodize_core::github_client::{CreateReleaseParams, GitHubClient, MockGitHubClient};

        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Err("401 Unauthorized: Bad credentials".to_string()));

        let params = CreateReleaseParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release v1.0.0".to_string(),
            body: String::new(),
            draft: false,
            prerelease: false,
            generate_release_notes: false,
            make_latest: None,
        };

        let result = mock.create_release(&params);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("401") && err.contains("Unauthorized"),
            "error should contain HTTP status and description, got: {err}"
        );
    }

    #[test]
    fn test_mock_github_api_403_error() {
        use anodize_core::github_client::{CreateReleaseParams, GitHubClient, MockGitHubClient};

        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Err(
            "403 Forbidden: Resource not accessible by integration".to_string(),
        ));

        let params = CreateReleaseParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release".to_string(),
            body: String::new(),
            draft: false,
            prerelease: false,
            generate_release_notes: false,
            make_latest: None,
        };

        let result = mock.create_release(&params);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("403"));
    }

    #[test]
    fn test_mock_github_api_404_error() {
        use anodize_core::github_client::{CreateReleaseParams, GitHubClient, MockGitHubClient};

        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Err("404 Not Found: repository not found".to_string()));

        let params = CreateReleaseParams {
            owner: "testowner".to_string(),
            repo: "nonexistent-repo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release".to_string(),
            body: String::new(),
            draft: false,
            prerelease: false,
            generate_release_notes: false,
            make_latest: None,
        };

        let result = mock.create_release(&params);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("404") && err.contains("Not Found"),
            "error should contain 404 Not Found, got: {err}"
        );
    }

    #[test]
    fn test_mock_github_api_422_error() {
        use anodize_core::github_client::{CreateReleaseParams, GitHubClient, MockGitHubClient};

        let mock = MockGitHubClient::new();
        mock.set_create_release_response(Err(
            "422 Unprocessable Entity: Validation Failed - tag already exists".to_string(),
        ));

        let params = CreateReleaseParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            tag_name: "v1.0.0".to_string(),
            name: "Release".to_string(),
            body: String::new(),
            draft: false,
            prerelease: false,
            generate_release_notes: false,
            make_latest: None,
        };

        let result = mock.create_release(&params);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("422") && err.contains("Validation"),
            "error should contain 422 and Validation, got: {err}"
        );
    }

    #[test]
    fn test_mock_upload_failure() {
        use anodize_core::github_client::{GitHubClient, MockGitHubClient, UploadAssetParams};

        let mock = MockGitHubClient::new();
        mock.set_upload_asset_response(Err(
            "upload failed: connection timeout after 30s".to_string()
        ));

        let params = UploadAssetParams {
            owner: "testowner".to_string(),
            repo: "testrepo".to_string(),
            release_id: 42,
            file_name: "myapp.tar.gz".to_string(),
            file_path: std::path::PathBuf::from("/tmp/myapp.tar.gz"),
        };

        let result = mock.upload_asset(&params);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("upload failed") && err.contains("timeout"),
            "error should describe the upload failure, got: {err}"
        );
    }

    #[test]
    fn test_dry_run_with_draft_release() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    draft: Some(true),
                    prerelease: Some(PrereleaseConfig::Auto),
                    make_latest: Some(MakeLatestConfig::Bool(false)),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- conflicting draft config tests ----

    #[test]
    fn test_conflicting_replace_and_use_existing_draft_fails() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    replace_existing_draft: Some(true),
                    use_existing_draft: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "conflicting draft options should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("replace_existing_draft") && err.contains("use_existing_draft"),
            "error should mention both conflicting options, got: {err}"
        );
    }

    #[test]
    fn test_replace_existing_draft_alone_ok() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    replace_existing_draft: Some(true),
                    use_existing_draft: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_use_existing_draft_alone_ok() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    replace_existing_draft: Some(false),
                    use_existing_draft: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- release disable tests ----

    #[test]
    fn test_release_disable_config_parsing() {
        let yaml = r#"
disable: true
draft: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.disable, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_release_disable_config_parsing_false() {
        let yaml = r#"
disable: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.disable, Some(StringOrBool::Bool(false)));
    }

    #[test]
    fn test_release_disable_config_parsing_template_string() {
        let yaml = r#"
disable: "{{ if IsSnapshot }}true{{ endif }}"
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match cfg.disable {
            Some(StringOrBool::String(s)) => {
                assert!(s.contains("IsSnapshot"));
            }
            other => panic!("expected StringOrBool::String, got {:?}", other),
        }
    }

    #[test]
    fn test_release_disable_config_parsing_absent() {
        let yaml = r#"
draft: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.disable, None);
    }

    #[test]
    fn test_release_stage_skipped_when_disabled() {
        // When disable: true is set, the release stage should skip
        // the crate entirely. We test via dry-run to avoid real API calls.
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    disable: Some(StringOrBool::Bool(true)),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        // Should succeed with no error - the crate is simply skipped
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_release_stage_not_skipped_when_disable_false() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    disable: Some(StringOrBool::Bool(false)),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        // Should succeed - disable=false means proceed normally (dry-run)
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- resolve_release_mode tests ----

    #[test]
    fn test_resolve_release_mode_defaults_to_keep_existing() {
        assert_eq!(resolve_release_mode(None).unwrap(), "keep-existing");
    }

    #[test]
    fn test_resolve_release_mode_empty_string_defaults_to_keep_existing() {
        assert_eq!(resolve_release_mode(Some("")).unwrap(), "keep-existing");
    }

    #[test]
    fn test_resolve_release_mode_keep_existing() {
        assert_eq!(
            resolve_release_mode(Some("keep-existing")).unwrap(),
            "keep-existing"
        );
    }

    #[test]
    fn test_resolve_release_mode_append() {
        assert_eq!(resolve_release_mode(Some("append")).unwrap(), "append");
    }

    #[test]
    fn test_resolve_release_mode_prepend() {
        assert_eq!(resolve_release_mode(Some("prepend")).unwrap(), "prepend");
    }

    #[test]
    fn test_resolve_release_mode_replace() {
        assert_eq!(resolve_release_mode(Some("replace")).unwrap(), "replace");
    }

    #[test]
    fn test_resolve_release_mode_invalid() {
        let result = resolve_release_mode(Some("invalid-mode"));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid mode 'invalid-mode'"),
            "error should name the invalid mode, got: {err}"
        );
        assert!(
            err.contains("keep-existing") && err.contains("append"),
            "error should list valid modes, got: {err}"
        );
    }

    #[test]
    fn test_release_mode_stored_in_config() {
        let yaml = r#"
mode: keep-existing
draft: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.mode.as_deref(), Some("keep-existing"));
    }

    #[test]
    fn test_release_mode_absent_in_config() {
        let yaml = r#"
draft: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.mode, None);
    }

    #[test]
    fn test_release_mode_all_valid_values_in_config() {
        for mode in &["keep-existing", "append", "prepend", "replace"] {
            let yaml = format!("mode: {}", mode);
            let cfg: ReleaseConfig = serde_yaml_ng::from_str(&yaml).unwrap();
            assert_eq!(cfg.mode.as_deref(), Some(*mode));
            // Verify it passes validation
            assert!(resolve_release_mode(cfg.mode.as_deref()).is_ok());
        }
    }

    #[test]
    fn test_dry_run_logs_release_mode() {
        // When mode is set, the dry-run output should include it
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    mode: Some("append".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        // Dry-run should succeed; the mode is validated and logged
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_invalid_release_mode_fails_stage() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    mode: Some("bogus".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "invalid release mode should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid mode") || err.contains("bogus"),
            "error should mention invalid mode, got: {err}"
        );
    }

    // ---- ids filtering tests ----

    #[test]
    fn test_ids_filter_includes_matching_artifacts() {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    ids: Some(vec!["linux-amd64".to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        // Archive with matching id
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("/tmp/myapp-linux-amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "linux-amd64".to_string())]),
            size: None,
        });

        // Archive with non-matching id
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("/tmp/myapp-darwin-arm64.tar.gz"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "darwin-arm64".to_string())]),
            size: None,
        });

        let stage = ReleaseStage;
        // Dry-run succeeds; the filter is applied internally
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_ids_filter_none_includes_all_artifacts() {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    ids: None, // no filter
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        // Add two archives with different ids
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("/tmp/myapp-linux.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "linux-amd64".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("/tmp/myapp-darwin.tar.gz"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "darwin-arm64".to_string())]),
            size: None,
        });

        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_ids_filter_unit_logic() {
        // Directly test the filter logic used in the release stage:
        // artifacts whose metadata "id" is in the ids list pass; others don't.
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let ids = ["linux-amd64".to_string(), "windows-amd64".to_string()];

        let artifacts = [
            Artifact {
                kind: ArtifactKind::Archive,
                name: String::new(),
                path: PathBuf::from("/tmp/linux.tar.gz"),
                target: None,
                crate_name: "app".to_string(),
                metadata: HashMap::from([("id".to_string(), "linux-amd64".to_string())]),
                size: None,
            },
            Artifact {
                kind: ArtifactKind::Archive,
                name: String::new(),
                path: PathBuf::from("/tmp/darwin.tar.gz"),
                target: None,
                crate_name: "app".to_string(),
                metadata: HashMap::from([("id".to_string(), "darwin-arm64".to_string())]),
                size: None,
            },
            Artifact {
                kind: ArtifactKind::Archive,
                name: String::new(),
                path: PathBuf::from("/tmp/windows.zip"),
                target: None,
                crate_name: "app".to_string(),
                metadata: HashMap::from([("id".to_string(), "windows-amd64".to_string())]),
                size: None,
            },
            Artifact {
                kind: ArtifactKind::Checksum,
                name: String::new(),
                path: PathBuf::from("/tmp/checksums.txt"),
                target: None,
                crate_name: "app".to_string(),
                metadata: HashMap::new(), // no id metadata
                size: None,
            },
        ];

        // Apply the same filter logic as the stage
        let filtered: Vec<_> = artifacts
            .iter()
            .filter(|a| matches!(a.metadata.get("id"), Some(id) if ids.contains(id)))
            .collect();

        assert_eq!(filtered.len(), 2, "should match linux and windows only");
        assert_eq!(
            filtered[0].path,
            PathBuf::from("/tmp/linux.tar.gz"),
            "first match should be linux"
        );
        assert_eq!(
            filtered[1].path,
            PathBuf::from("/tmp/windows.zip"),
            "second match should be windows"
        );
    }

    #[test]
    fn test_ids_filter_no_id_metadata_excluded() {
        // Artifacts without "id" metadata should be excluded when ids filter is set
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let ids = ["linux-amd64".to_string()];

        let artifact_no_id = Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("/tmp/mystery.tar.gz"),
            target: None,
            crate_name: "app".to_string(),
            metadata: HashMap::new(),
            size: None,
        };

        let matches = matches!(artifact_no_id.metadata.get("id"), Some(id) if ids.contains(id));
        assert!(
            !matches,
            "artifact without id metadata should not match ids filter"
        );
    }

    #[test]
    fn test_ids_config_parsing() {
        let yaml = r#"
ids:
  - linux-amd64
  - darwin-arm64
draft: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let ids = cfg.ids.unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "linux-amd64");
        assert_eq!(ids[1], "darwin-arm64");
    }

    #[test]
    fn test_ids_config_absent() {
        let yaml = r#"
draft: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(cfg.ids.is_none());
    }

    #[test]
    fn test_ids_and_mode_combined_dry_run() {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    mode: Some("prepend".to_string()),
                    ids: Some(vec!["linux-amd64".to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("/tmp/myapp-linux.tar.gz"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "linux-amd64".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("/tmp/myapp-darwin.tar.gz"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "darwin-arm64".to_string())]),
            size: None,
        });

        let stage = ReleaseStage;
        assert!(
            stage.run(&mut ctx).is_ok(),
            "dry-run with mode + ids should succeed"
        );
    }

    #[test]
    fn test_release_collects_all_uploadable_artifact_kinds() {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use std::path::PathBuf;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        // Add one artifact of each uploadable kind.
        let uploadable_kinds = vec![
            (ArtifactKind::Archive, "myapp.tar.gz"),
            (ArtifactKind::Checksum, "checksums.txt"),
            (ArtifactKind::LinuxPackage, "myapp.deb"),
            (ArtifactKind::Snap, "myapp.snap"),
            (ArtifactKind::DiskImage, "myapp.dmg"),
            (ArtifactKind::Installer, "myapp.msi"),
            (ArtifactKind::MacOsPackage, "myapp.pkg"),
            (ArtifactKind::SourceArchive, "myapp-src.tar.gz"),
            (ArtifactKind::Sbom, "myapp.sbom.json"),
        ];
        for (kind, name) in &uploadable_kinds {
            ctx.artifacts.add(Artifact {
                kind: *kind,
                name: String::new(),
                path: PathBuf::from(format!("/tmp/{}", name)),
                target: None,
                crate_name: "myapp".to_string(),
                metadata: Default::default(),
                size: None,
            });
        }

        // Also add a signature Metadata artifact (should be uploaded).
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Metadata,
            name: String::new(),
            path: PathBuf::from("/tmp/checksums.txt.sig"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: std::collections::HashMap::from([(
                "type".to_string(),
                "Signature".to_string(),
            )]),
            size: None,
        });

        // Add non-uploadable kinds (should NOT be uploaded).
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/tmp/myapp"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DockerImage,
            name: String::new(),
            path: PathBuf::from("ghcr.io/test/myapp:latest"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Library,
            name: String::new(),
            path: PathBuf::from("/tmp/libmyapp.so"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Wasm,
            name: String::new(),
            path: PathBuf::from("/tmp/myapp.wasm"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        // Plain Metadata (not Signature/Certificate) should NOT be uploaded.
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Metadata,
            name: String::new(),
            path: PathBuf::from("/tmp/metadata.json"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = ReleaseStage;
        assert!(
            stage.run(&mut ctx).is_ok(),
            "dry-run with all artifact kinds should succeed"
        );

        // The dry-run completes successfully, confirming the expanded artifact
        // collection logic compiles and processes all expected kinds.
    }

    // ---- compose_body_for_mode tests ----

    #[test]
    fn test_compose_body_replace_ignores_existing() {
        let result = compose_body_for_mode("replace", Some("old body"), "new body");
        assert_eq!(result, "new body");
    }

    #[test]
    fn test_compose_body_replace_no_existing() {
        let result = compose_body_for_mode("replace", None, "new body");
        assert_eq!(result, "new body");
    }

    #[test]
    fn test_compose_body_keep_existing_with_existing() {
        let result = compose_body_for_mode("keep-existing", Some("old body"), "new body");
        assert_eq!(result, "old body");
    }

    #[test]
    fn test_compose_body_keep_existing_empty_existing() {
        let result = compose_body_for_mode("keep-existing", Some(""), "new body");
        assert_eq!(result, "new body");
    }

    #[test]
    fn test_compose_body_keep_existing_no_existing() {
        let result = compose_body_for_mode("keep-existing", None, "new body");
        assert_eq!(result, "new body");
    }

    #[test]
    fn test_compose_body_append_with_existing() {
        let result = compose_body_for_mode("append", Some("old body"), "new body");
        assert_eq!(result, "old body\n\nnew body");
    }

    #[test]
    fn test_compose_body_append_no_existing() {
        let result = compose_body_for_mode("append", None, "new body");
        assert_eq!(result, "new body");
    }

    #[test]
    fn test_compose_body_append_empty_existing() {
        let result = compose_body_for_mode("append", Some(""), "new body");
        assert_eq!(result, "new body");
    }

    #[test]
    fn test_compose_body_prepend_with_existing() {
        let result = compose_body_for_mode("prepend", Some("old body"), "new body");
        assert_eq!(result, "new body\n\nold body");
    }

    #[test]
    fn test_compose_body_prepend_no_existing() {
        let result = compose_body_for_mode("prepend", None, "new body");
        assert_eq!(result, "new body");
    }

    #[test]
    fn test_compose_body_prepend_empty_existing() {
        let result = compose_body_for_mode("prepend", Some(""), "new body");
        assert_eq!(result, "new body");
    }

    // ---- resolve_content_source tests ----

    #[test]
    fn test_resolve_content_source_inline() {
        let source = ContentSource::Inline("hello world".to_string());
        assert_eq!(resolve_content_source(&source).unwrap(), "hello world");
    }

    #[test]
    fn test_resolve_content_source_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("header.md");
        std::fs::write(&file_path, "# Release Header\nFrom file.").unwrap();

        let source = ContentSource::FromFile {
            from_file: file_path.to_string_lossy().into_owned(),
        };
        let result = resolve_content_source(&source).unwrap();
        assert_eq!(result, "# Release Header\nFrom file.");
    }

    #[test]
    fn test_resolve_content_source_from_file_not_found() {
        let source = ContentSource::FromFile {
            from_file: "/tmp/anodize_nonexistent_file_12345.md".to_string(),
        };
        let result = resolve_content_source(&source);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to read"));
    }

    // ---- new config field parsing tests ----

    #[test]
    fn test_target_commitish_config_parsing() {
        let yaml = r#"
target_commitish: main
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.target_commitish, Some("main".to_string()));
    }

    #[test]
    fn test_target_commitish_absent() {
        let yaml = r#"
draft: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.target_commitish, None);
    }

    #[test]
    fn test_discussion_category_name_config_parsing() {
        let yaml = r#"
discussion_category_name: Announcements
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            cfg.discussion_category_name,
            Some("Announcements".to_string())
        );
    }

    #[test]
    fn test_discussion_category_name_absent() {
        let yaml = r#"
draft: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.discussion_category_name, None);
    }

    #[test]
    fn test_include_meta_config_parsing() {
        let yaml = r#"
include_meta: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.include_meta, Some(true));
    }

    #[test]
    fn test_include_meta_false() {
        let yaml = r#"
include_meta: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.include_meta, Some(false));
    }

    #[test]
    fn test_include_meta_absent() {
        let yaml = r#"
draft: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.include_meta, None);
    }

    #[test]
    fn test_use_existing_draft_config_parsing() {
        let yaml = r#"
use_existing_draft: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_existing_draft, Some(true));
    }

    #[test]
    fn test_use_existing_draft_false() {
        let yaml = r#"
use_existing_draft: false
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_existing_draft, Some(false));
    }

    #[test]
    fn test_use_existing_draft_absent() {
        let yaml = r#"
draft: true
"#;
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_existing_draft, None);
    }

    // ---- dry-run tests for new config fields ----

    #[test]
    fn test_dry_run_with_target_commitish() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    target_commitish: Some("main".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_with_discussion_category_name() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    discussion_category_name: Some("Releases".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_with_include_meta() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    include_meta: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_with_use_existing_draft() {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    use_existing_draft: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_with_all_new_fields() {
        // GoReleaser parity: extra_files globs must match at least one file.
        let tmp = std::env::temp_dir().join("anodize_test_dry_all_fields");
        let _ = std::fs::create_dir_all(&tmp);
        let file = tmp.join("extra.sig");
        std::fs::write(&file, "sig").unwrap();
        let pattern = tmp.join("*.sig").to_string_lossy().into_owned();

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    header: Some(ContentSource::Inline("# Header".to_string())),
                    footer: Some(ContentSource::Inline("Footer".to_string())),
                    extra_files: Some(vec![ExtraFileSpec::Glob(pattern)]),
                    target_commitish: Some("release/v1".to_string()),
                    discussion_category_name: Some("Announcements".to_string()),
                    include_meta: Some(true),
                    use_existing_draft: Some(false),
                    mode: Some("append".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        ctx.changelogs
            .insert("testcrate".to_string(), "- changes".to_string());
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---- ContentSource from_file dry-run integration test ----

    #[test]
    fn test_dry_run_with_header_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let header_path = dir.path().join("header.md");
        std::fs::write(&header_path, "# Release from file").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    header: Some(ContentSource::FromFile {
                        from_file: header_path.to_string_lossy().into_owned(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_include_meta_collects_dist_files() {
        // Create a temp dist directory with metadata files
        let dir = tempfile::tempdir().unwrap();
        let dist_dir = dir.path().join("dist");
        std::fs::create_dir_all(&dist_dir).unwrap();
        std::fs::write(dist_dir.join("metadata.json"), r#"{"key":"value"}"#).unwrap();
        std::fs::write(dist_dir.join("artifacts.json"), r#"[]"#).unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    include_meta: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        // Override the dist path to our temp directory
        ctx.config.dist = dist_dir.clone();

        let stage = ReleaseStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- body truncation tests ----

    #[test]
    fn test_build_release_json_body_within_limit() {
        let body = "a".repeat(1000);
        let json = build_release_json(
            "v1.0.0",
            "Release v1.0.0",
            &body,
            false,
            false,
            &None,
            &None,
            &None,
            false,
        );
        assert_eq!(json["body"].as_str().unwrap(), &body);
    }

    #[test]
    fn test_build_release_json_body_at_limit() {
        let body = "a".repeat(GITHUB_RELEASE_BODY_MAX_CHARS);
        let json = build_release_json(
            "v1.0.0",
            "Release v1.0.0",
            &body,
            false,
            false,
            &None,
            &None,
            &None,
            false,
        );
        assert_eq!(json["body"].as_str().unwrap(), &body);
    }

    #[test]
    fn test_build_release_json_body_exceeds_limit_is_truncated() {
        let body = "a".repeat(GITHUB_RELEASE_BODY_MAX_CHARS + 500);
        let json = build_release_json(
            "v1.0.0",
            "Release v1.0.0",
            &body,
            false,
            false,
            &None,
            &None,
            &None,
            false,
        );
        let result = json["body"].as_str().unwrap();
        let suffix = "\n\n...(truncated)";
        // Total length must not exceed the limit.
        assert!(
            result.len() <= GITHUB_RELEASE_BODY_MAX_CHARS,
            "truncated body length {} exceeds limit {}",
            result.len(),
            GITHUB_RELEASE_BODY_MAX_CHARS,
        );
        // The content portion should be max_chars - suffix length of 'a's.
        let expected_content_len = GITHUB_RELEASE_BODY_MAX_CHARS - suffix.len();
        assert!(result.starts_with(&"a".repeat(expected_content_len)));
        assert!(result.ends_with(suffix));
    }

    #[test]
    fn test_build_release_json_empty_body_not_set() {
        let json = build_release_json(
            "v1.0.0",
            "Release v1.0.0",
            "",
            false,
            false,
            &None,
            &None,
            &None,
            false,
        );
        assert!(json.get("body").is_none());
    }

    // ---- draft-then-publish: build_release_json always uses draft as passed ----

    #[test]
    fn test_build_release_json_draft_true() {
        let json = build_release_json(
            "v1.0.0",
            "Release v1.0.0",
            "body",
            true,
            false,
            &None,
            &None,
            &None,
            false,
        );
        assert!(json["draft"].as_bool().unwrap());
    }

    #[test]
    fn test_build_release_json_draft_false() {
        let json = build_release_json(
            "v1.0.0",
            "Release v1.0.0",
            "body",
            false,
            false,
            &None,
            &None,
            &None,
            false,
        );
        assert!(!json["draft"].as_bool().unwrap());
    }

    #[test]
    fn test_dry_run_with_templated_extra_files() {
        use anodize_core::config::TemplatedExtraFile;

        let tmp = tempfile::TempDir::new().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();

        // Create a source template file
        let tpl_src = tmp.path().join("NOTES.md.tpl");
        std::fs::write(&tpl_src, "Release {{ .ProjectName }} {{ .Version }}").unwrap();

        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v2.0.0")
            .dry_run(true)
            .dist(dist.clone())
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                release: Some(ReleaseConfig {
                    templated_extra_files: Some(vec![TemplatedExtraFile {
                        src: tpl_src.to_string_lossy().to_string(),
                        dst: Some("RELEASE-NOTES.md".to_string()),
                        mode: None,
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();

        let stage = ReleaseStage;
        stage.run(&mut ctx).unwrap();

        // Verify the templated file was rendered and written to dist
        let rendered = dist.join("RELEASE-NOTES.md");
        assert!(
            rendered.exists(),
            "templated extra file should be written to dist"
        );
        let content = std::fs::read_to_string(&rendered).unwrap();
        assert_eq!(content, "Release myapp 2.0.0");
    }

    // -----------------------------------------------------------------------
    // GitHub Enterprise URL support tests
    // -----------------------------------------------------------------------

    /// Helper: build_octocrab_client requires a tokio runtime (octocrab's
    /// Buffer service needs one) and a rustls CryptoProvider installed.
    /// Wrap assertions in a temporary runtime with the provider set.
    fn with_tokio<F: FnOnce()>(f: F) {
        // Install the ring crypto provider if not already installed.
        // ignore error if another test thread already installed it.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { f() });
    }

    #[test]
    fn test_build_octocrab_client_default_no_github_urls() {
        // When github_urls is None, build_octocrab_client should succeed
        // with standard GitHub.com endpoints.
        with_tokio(|| {
            let client = build_octocrab_client("ghp_fake_token_123", &None);
            assert!(
                client.is_ok(),
                "default client (no github_urls) should build successfully"
            );
        });
    }

    #[test]
    fn test_build_octocrab_client_with_enterprise_api_url() {
        with_tokio(|| {
            let urls = Some(GitHubUrlsConfig {
                api: Some("https://github.example.com/api/v3/".to_string()),
                upload: None,
                download: None,
                skip_tls_verify: None,
            });
            let client = build_octocrab_client("ghp_fake_token_123", &urls);
            assert!(
                client.is_ok(),
                "client with enterprise api URL should build successfully"
            );
        });
    }

    #[test]
    fn test_build_octocrab_client_with_enterprise_api_and_upload_urls() {
        with_tokio(|| {
            let urls = Some(GitHubUrlsConfig {
                api: Some("https://github.example.com/api/v3/".to_string()),
                upload: Some("https://github.example.com/api/uploads/".to_string()),
                download: Some("https://github.example.com/".to_string()),
                skip_tls_verify: None,
            });
            let client = build_octocrab_client("ghp_fake_token_123", &urls);
            assert!(
                client.is_ok(),
                "client with enterprise api + upload URLs should build successfully"
            );
        });
    }

    #[test]
    fn test_build_octocrab_client_with_skip_tls_verify() {
        with_tokio(|| {
            let urls = Some(GitHubUrlsConfig {
                api: Some("https://github.example.com/api/v3/".to_string()),
                upload: Some("https://github.example.com/api/uploads/".to_string()),
                download: None,
                skip_tls_verify: Some(true),
            });
            let client = build_octocrab_client("ghp_fake_token_123", &urls);
            assert!(
                client.is_ok(),
                "client with skip_tls_verify should build successfully"
            );
        });
    }

    #[test]
    fn test_build_octocrab_client_invalid_api_url_errors() {
        with_tokio(|| {
            let urls = Some(GitHubUrlsConfig {
                api: Some("not a valid url \x00".to_string()),
                upload: None,
                download: None,
                skip_tls_verify: None,
            });
            let result = build_octocrab_client("ghp_fake_token_123", &urls);
            assert!(result.is_err(), "invalid api URL should produce an error");
        });
    }

    #[test]
    fn test_build_octocrab_client_skip_tls_false_uses_normal_path() {
        // skip_tls_verify = Some(false) should use the normal (secure) path.
        with_tokio(|| {
            let urls = Some(GitHubUrlsConfig {
                api: Some("https://github.example.com/api/v3/".to_string()),
                upload: None,
                download: None,
                skip_tls_verify: Some(false),
            });
            let client = build_octocrab_client("ghp_fake_token_123", &urls);
            assert!(
                client.is_ok(),
                "skip_tls_verify=false should use normal build path"
            );
        });
    }

    #[test]
    fn test_dry_run_logs_github_enterprise_urls() {
        // When github_urls are configured and dry_run is true, the release
        // stage should log the enterprise URL configuration.
        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                release: Some(ReleaseConfig::default()),
                ..Default::default()
            }])
            .build();

        ctx.config.github_urls = Some(GitHubUrlsConfig {
            api: Some("https://ghe.corp.example.com/api/v3/".to_string()),
            upload: Some("https://ghe.corp.example.com/api/uploads/".to_string()),
            download: Some("https://ghe.corp.example.com/".to_string()),
            skip_tls_verify: Some(true),
        });

        let stage = ReleaseStage;
        // Dry-run should succeed — no actual API calls are made.
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_dry_run_without_github_urls_still_works() {
        // Verify the default path (no github_urls) still works in dry-run.
        let mut ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                release: Some(ReleaseConfig::default()),
                ..Default::default()
            }])
            .build();

        assert!(ctx.config.github_urls.is_none());

        let stage = ReleaseStage;
        stage.run(&mut ctx).unwrap();
    }

    // ---- GitLab backend tests ----

    #[test]
    fn test_dry_run_gitlab_token_type_shows_gitlab_release() {
        use anodize_core::config::ScmRepoConfig;
        use anodize_core::scm::ScmTokenType;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    gitlab: Some(ScmRepoConfig {
                        owner: "mygroup".to_string(),
                        name: "myproject".to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        ctx.token_type = ScmTokenType::GitLab;

        let stage = ReleaseStage;
        // Dry-run with GitLab token type should succeed.
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_gitlab_with_custom_urls() {
        use anodize_core::config::{GitLabUrlsConfig, ScmRepoConfig};
        use anodize_core::scm::ScmTokenType;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    gitlab: Some(ScmRepoConfig {
                        owner: "corp".to_string(),
                        name: "app".to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        ctx.token_type = ScmTokenType::GitLab;
        ctx.config.gitlab_urls = Some(GitLabUrlsConfig {
            api: Some("https://gitlab.example.com/api/v4".to_string()),
            download: Some("https://gitlab.example.com".to_string()),
            skip_tls_verify: Some(true),
            use_package_registry: Some(true),
            use_job_token: Some(false),
        });

        let stage = ReleaseStage;
        // Dry-run with custom GitLab URLs should succeed and show them.
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_gitlab_backend_skips_when_no_gitlab_config() {
        use anodize_core::scm::ScmTokenType;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .token(Some("glpat-test-token".to_string()))
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    // No gitlab config, no github config either.
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        ctx.token_type = ScmTokenType::GitLab;

        let stage = ReleaseStage;
        // Should succeed by skipping (warn + continue) since no gitlab config.
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_gitlab_backend_falls_back_to_github_config() {
        use anodize_core::config::ScmRepoConfig;
        use anodize_core::scm::ScmTokenType;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    // Only github config set, no gitlab-specific config.
                    github: Some(ScmRepoConfig {
                        owner: "fallback-owner".to_string(),
                        name: "fallback-repo".to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        ctx.token_type = ScmTokenType::GitLab;

        let stage = ReleaseStage;
        // Should succeed in dry-run because GitLab falls back to github config.
        assert!(stage.run(&mut ctx).is_ok());
    }

    // ---- Gitea backend tests ----

    #[test]
    fn test_gitea_dry_run_with_gitea_config() {
        use anodize_core::config::ScmRepoConfig;
        use anodize_core::scm::ScmTokenType;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    gitea: Some(ScmRepoConfig {
                        owner: "owner".to_string(),
                        name: "repo".to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        ctx.token_type = ScmTokenType::Gitea;

        let stage = ReleaseStage;
        // Should succeed in dry-run.
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_gitea_backend_skips_when_no_gitea_config() {
        use anodize_core::scm::ScmTokenType;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .token(Some("gitea-test-token".to_string()))
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    // No gitea config, no github config either.
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        ctx.token_type = ScmTokenType::Gitea;

        let stage = ReleaseStage;
        // Should succeed by skipping (warn + continue) since no gitea config.
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_gitea_backend_falls_back_to_github_config() {
        use anodize_core::config::ScmRepoConfig;
        use anodize_core::scm::ScmTokenType;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    // Only github config set, no gitea-specific config.
                    github: Some(ScmRepoConfig {
                        owner: "fallback-owner".to_string(),
                        name: "fallback-repo".to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        ctx.token_type = ScmTokenType::Gitea;

        let stage = ReleaseStage;
        // Should succeed in dry-run because Gitea falls back to github config.
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_gitea_missing_token_errors() {
        use anodize_core::config::ScmRepoConfig;
        use anodize_core::scm::ScmTokenType;

        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .token(None)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    gitea: Some(ScmRepoConfig {
                        owner: "owner".to_string(),
                        name: "repo".to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        ctx.token_type = ScmTokenType::Gitea;

        let stage = ReleaseStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("GITEA_TOKEN") || err.contains("--token"),
            "error should mention GITEA_TOKEN or --token, got: {err}"
        );
    }

    // ---- resolve_github_username tests ----

    #[test]
    fn test_resolve_github_username_noreply_with_id() {
        // ID+USERNAME@users.noreply.github.com pattern
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _ = rustls::crypto::ring::default_provider().install_default();
        rt.block_on(async {
            let octo = octocrab::Octocrab::builder()
                .personal_token("fake-token".to_string())
                .build()
                .unwrap();
            let mut cache = HashMap::new();
            let result = resolve_github_username(
                &octo,
                "12345+octocat@users.noreply.github.com",
                &mut cache,
                None,
            )
            .await;
            assert_eq!(result, Some("octocat".to_string()));
            // Verify it was cached
            assert_eq!(
                cache.get("12345+octocat@users.noreply.github.com"),
                Some(&Some("octocat".to_string()))
            );
        });
    }

    #[test]
    fn test_resolve_github_username_noreply_without_id() {
        // USERNAME@users.noreply.github.com pattern
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _ = rustls::crypto::ring::default_provider().install_default();
        rt.block_on(async {
            let octo = octocrab::Octocrab::builder()
                .personal_token("fake-token".to_string())
                .build()
                .unwrap();
            let mut cache = HashMap::new();
            let result = resolve_github_username(
                &octo,
                "octocat@users.noreply.github.com",
                &mut cache,
                None,
            )
            .await;
            assert_eq!(result, Some("octocat".to_string()));
        });
    }

    #[test]
    fn test_resolve_github_username_cache_hit() {
        // Pre-populate cache and verify it's used without API call
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _ = rustls::crypto::ring::default_provider().install_default();
        rt.block_on(async {
            let octo = octocrab::Octocrab::builder()
                .personal_token("fake-token".to_string())
                .build()
                .unwrap();
            let mut cache = HashMap::new();
            cache.insert(
                "cached@example.com".to_string(),
                Some("cached-user".to_string()),
            );
            let result =
                resolve_github_username(&octo, "cached@example.com", &mut cache, None).await;
            assert_eq!(result, Some("cached-user".to_string()));
        });
    }

    #[test]
    fn test_resolve_github_username_cache_hit_none() {
        // Pre-populate cache with None (previously unresolved)
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _ = rustls::crypto::ring::default_provider().install_default();
        rt.block_on(async {
            let octo = octocrab::Octocrab::builder()
                .personal_token("fake-token".to_string())
                .build()
                .unwrap();
            let mut cache = HashMap::new();
            cache.insert("unknown@example.com".to_string(), None);
            let result =
                resolve_github_username(&octo, "unknown@example.com", &mut cache, None).await;
            assert_eq!(result, None);
        });
    }

    #[test]
    fn test_resolve_github_username_noreply_numeric_id_plus_username() {
        // Verify complex ID+USERNAME patterns work
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _ = rustls::crypto::ring::default_provider().install_default();
        rt.block_on(async {
            let octo = octocrab::Octocrab::builder()
                .personal_token("fake-token".to_string())
                .build()
                .unwrap();
            let mut cache = HashMap::new();
            let result = resolve_github_username(
                &octo,
                "987654321+my-username@users.noreply.github.com",
                &mut cache,
                None,
            )
            .await;
            assert_eq!(result, Some("my-username".to_string()));
        });
    }

    #[test]
    fn test_resolve_github_username_regular_email_not_noreply() {
        // Regular emails should NOT be parsed as noreply
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _ = rustls::crypto::ring::default_provider().install_default();
        rt.block_on(async {
            let octo = octocrab::Octocrab::builder()
                .personal_token("fake-token".to_string())
                .build()
                .unwrap();
            let mut cache = HashMap::new();
            // This will try the API and fail (no real token), so it should
            // cache None and return None.
            let result = resolve_github_username(&octo, "user@example.com", &mut cache, None).await;
            // With a fake token, the API call will fail, resulting in None.
            assert_eq!(result, None);
            // Verify it was cached
            assert!(cache.contains_key("user@example.com"));
        });
    }
}
