//! Native nupkg creation (OPC/ZIP) + NuGet V2 feed query/push.
//!
//! A `.nupkg` is an Open Packaging Conventions archive — a ZIP with
//! `[Content_Types].xml` and `_rels/.rels` at the root, the nuspec
//! manifest, and `tools/**` content. Building it natively lets us avoid
//! the Windows-only `choco pack` CLI.

use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryPolicy, SuccessClass, retry_http_blocking};
use anyhow::{Context as _, Result};

/// Content types XML — required by the OPC (Open Packaging Conventions) spec.
/// Maps file extensions to MIME types within the package.
const CONTENT_TYPES_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml" />
  <Default Extension="nuspec" ContentType="application/octet-stream" />
  <Default Extension="ps1" ContentType="application/octet-stream" />
  <Default Extension="psmdcp" ContentType="application/vnd.openxmlformats-package.core-properties+xml" />
</Types>"#;

/// Package relationships XML — links the nuspec as the package manifest.
fn rels_xml(nuspec_filename: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Type="http://schemas.microsoft.com/packaging/2010/07/manifest" Target="/{}" Id="R1" />
</Relationships>"#,
        nuspec_filename
    )
}

/// Create a .nupkg file (OPC-compliant ZIP) from a nuspec and tools directory.
///
/// The nupkg format is an Open Packaging Conventions (OPC) archive:
/// - `[Content_Types].xml` — MIME type mappings
/// - `_rels/.rels` — package relationships (points to the nuspec)
/// - `{name}.nuspec` — NuGet/Chocolatey package manifest
/// - `tools/**` — package content (install scripts, binaries)
///
/// This replaces the `choco pack` CLI command with native Rust ZIP creation,
/// eliminating the dependency on the Windows-only Chocolatey CLI.
pub(super) fn create_nupkg(
    name: &str,
    nuspec_path: &std::path::Path,
    tools_dir: &std::path::Path,
    output_path: &std::path::Path,
) -> Result<()> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let nuspec_content = std::fs::read(nuspec_path)
        .with_context(|| format!("chocolatey: read nuspec {}", nuspec_path.display()))?;

    let file = std::fs::File::create(output_path)
        .with_context(|| format!("chocolatey: create nupkg {}", output_path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    // [Content_Types].xml (must be at root of ZIP)
    zip.start_file("[Content_Types].xml", options)?;
    zip.write_all(CONTENT_TYPES_XML.as_bytes())?;

    // _rels/.rels
    let nuspec_filename = format!("{}.nuspec", name);
    zip.start_file("_rels/.rels", options)?;
    zip.write_all(rels_xml(&nuspec_filename).as_bytes())?;

    // {name}.nuspec
    zip.start_file(&nuspec_filename, options)?;
    zip.write_all(&nuspec_content)?;

    // tools/** — walk the tools directory and add all files
    if tools_dir.exists() {
        for entry in walkdir(tools_dir)? {
            let rel_path = entry
                .strip_prefix(tools_dir.parent().unwrap_or(tools_dir))
                .unwrap_or(&entry);
            // Use forward slashes in ZIP paths (per ZIP spec and NuGet convention)
            let zip_path = rel_path.to_string_lossy().replace('\\', "/");
            let content = std::fs::read(&entry)
                .with_context(|| format!("chocolatey: read {}", entry.display()))?;
            zip.start_file(&zip_path, options)?;
            zip.write_all(&content)?;
        }
    }

    zip.finish()?;

    // Validate: the nupkg should be a valid ZIP with reasonable size
    let metadata = std::fs::metadata(output_path)?;
    if metadata.len() == 0 {
        anyhow::bail!(
            "chocolatey: generated nupkg is empty: {}",
            output_path.display()
        );
    }

    Ok(())
}

/// Recursively collect all files in a directory.
fn walkdir(dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("chocolatey: read dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(walkdir(&path)?);
        } else {
            files.push(path);
        }
    }
    Ok(files)
}

/// Outcome of checking the NuGet V2 feed for an existing package version.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FeedHashResult {
    /// Feed has this version. `status` and `is_approved` distinguish a
    /// published version from one stuck in the community moderation queue.
    ///
    /// The OData feed used by the Chocolatey community gallery does NOT
    /// emit `<d:Listed>` — moderation state is exposed only via
    /// `<d:PackageStatus>` ("Submitted" / "Approved" / "Rejected" /
    /// "Exempted" / "Unknown") and `<d:IsApproved>` (boolean). Live
    /// responses (2026-05-13):
    /// - in moderation: `PackageStatus=Submitted`, `IsApproved=false`
    /// - approved:      `PackageStatus=Approved`,  `IsApproved=true`
    Present {
        hash: String,
        algorithm: String,
        /// `<d:PackageStatus>`: "Submitted" / "Approved" / "Rejected" /
        /// "Exempted" / "Unknown". Absent on feeds that don't expose the
        /// field.
        status: Option<String>,
        /// `<d:IsApproved>` boolean. `Some(true)` for approved packages,
        /// `Some(false)` for any non-approved (typically moderation queue),
        /// `None` when the feed didn't expose the field.
        is_approved: Option<bool>,
        /// `<d:Published>` ISO-8601. The string "1900-01-01T00:00:00" is
        /// Chocolatey's unlisted sentinel.
        published: Option<String>,
    },
    /// Feed has this version but we could not parse a hash for it.
    PresentNoHash,
    /// Feed does not have this version (or we couldn't reach the feed).
    Absent,
}

/// Query the NuGet V2 OData feed for a package version and extract its
/// recorded hash so callers can detect drift between the local nupkg and
/// what's already published.
///
/// Chocolatey's community feed lives at `community.chocolatey.org`, but
/// pushes go to `push.chocolatey.org`. Map push URLs to the query feed so
/// the hash lookup works for either form.
///
/// The GET routes through [`retry_http_blocking`] so transient 5xx / 429 /
/// network failures retry per the user's top-level `retry:` policy. Any
/// non-recoverable failure (4xx, retry-exhaustion) maps to
/// [`FeedHashResult::Absent`] — same conservative "couldn't reach the
/// feed, fall through to push" behaviour as before.
pub(crate) fn package_feed_hash(
    push_source: &str,
    name: &str,
    version: &str,
    policy: &RetryPolicy,
) -> FeedHashResult {
    let query_base = if push_source.contains("push.chocolatey.org") {
        "https://community.chocolatey.org"
    } else {
        push_source.trim_end_matches('/')
    };
    // Normalize: strip any trailing /api/v2/package from push URLs.
    let query_base = query_base
        .trim_end_matches('/')
        .trim_end_matches("/api/v2/package")
        .trim_end_matches("/api/v2")
        .trim_end_matches('/');

    let url = format!(
        "{}/api/v2/Packages(Id='{}',Version='{}')",
        query_base, name, version
    );

    let client = match anodizer_core::http::blocking_client(std::time::Duration::from_secs(30)) {
        Ok(c) => c,
        Err(_) => return FeedHashResult::Absent,
    };

    let body = match retry_http_blocking(
        "chocolatey: feed hash lookup",
        policy,
        SuccessClass::Strict,
        |_| client.get(&url).send(),
        |status, body| {
            format!(
                "chocolatey: feed hash lookup returned HTTP {} for {}: {}",
                status,
                url,
                redact_bearer_tokens(body)
            )
        },
    ) {
        Ok((_, body)) => body,
        Err(_) => return FeedHashResult::Absent,
    };

    // Presence check: the OData feed returns a populated <entry> with
    // <id> and the version marker when the version is registered, and an
    // empty <feed> skeleton otherwise.
    let present = body.contains("<id>")
        && (body.contains(&format!(",Version='{}'", version))
            || body.contains(&format!("Version='{}'", version)));
    if !present {
        return FeedHashResult::Absent;
    }

    let hash = parse_xml_element(&body, "PackageHash");
    let algorithm = parse_xml_element(&body, "PackageHashAlgorithm");
    let status = parse_xml_element(&body, "PackageStatus");
    let is_approved = parse_xml_element(&body, "IsApproved").and_then(|v| v.parse::<bool>().ok());
    let published = parse_xml_element(&body, "Published");
    match (hash, algorithm) {
        (Some(h), Some(a)) if !h.is_empty() && !a.is_empty() => FeedHashResult::Present {
            hash: h,
            algorithm: a,
            status,
            is_approved,
            published,
        },
        _ => FeedHashResult::PresentNoHash,
    }
}

/// Classify a Present-feed-row's moderation state into a single triad:
/// `(label, in_moderation)`.
///
/// `label` is the short human-readable reason (e.g. "package in moderation
/// queue", "package rejected by moderator", "package approved"); the second
/// element is `true` when the row is in/awaiting moderation (i.e. a blocker
/// for both the preflight check and the publish step), `false` when it is
/// effectively visible (Approved, or unknown-but-row-exists — conservative).
///
/// Single source of truth for the two callsites — `preflight::Chocolatey`
/// and `chocolatey::publish` — that both need to decide whether a row in
/// the feed is "live" or "still in moderation".
pub(crate) fn classify_moderation(
    status: Option<&str>,
    is_approved: Option<bool>,
) -> (&'static str, bool) {
    // PackageStatus is the canonical discriminator; IsApproved is a fallback
    // when status is missing. The OData feed always emits PackageStatus for
    // rows that exist, but stay conservative.
    match status.map(|s| s.to_ascii_lowercase()) {
        Some(ref s) if s == "rejected" => ("package rejected by moderator", true),
        Some(ref s) if s == "submitted" || s == "unknown" => ("package in moderation queue", true),
        // Exempted = package is exempt from the standard moderation queue
        // (typically a trusted publisher or pre-approved package); the version
        // is immutable and live, not pending approval.
        Some(ref s) if s == "exempted" => ("package approved (exempted from moderation)", false),
        Some(ref s) if s == "approved" => ("package approved", false),
        _ => match is_approved {
            Some(false) => ("package in moderation queue", true),
            Some(true) => ("package approved", false),
            // Row exists but neither field present — conservatively treat
            // as visible (matches "at minimum, the row is on the feed").
            None => ("package on feed (status field absent)", false),
        },
    }
}

/// Extract the inner text of an OData property element. The feed uses
/// namespaced tag names like `<d:PackageHash>...</d:PackageHash>`; match
/// on the local part so the parse works regardless of the chosen prefix.
pub(super) fn parse_xml_element(body: &str, local_name: &str) -> Option<String> {
    // Find a tag whose local name matches (after any ':' prefix separator).
    let needle = format!("{}>", local_name);
    let mut search_from = 0;
    while let Some(tag_start) = body[search_from..].find(&needle) {
        let abs_tag_start = search_from + tag_start;
        // Make sure this is the opening tag — the char before must be '<'
        // or the local-name prefix boundary (':').
        let before = body[..abs_tag_start].chars().last();
        if !matches!(before, Some('<') | Some(':')) {
            search_from = abs_tag_start + needle.len();
            continue;
        }
        let value_start = abs_tag_start + needle.len();
        // Closing tag: look for "</..LocalName>".
        let closing_marker = format!("{}>", local_name);
        let rest = &body[value_start..];
        let close_idx = rest.find("</")?;
        let close_tag = &rest[close_idx..];
        if close_tag.contains(&closing_marker) {
            return Some(rest[..close_idx].trim().to_string());
        }
        search_from = abs_tag_start + needle.len();
    }
    None
}

/// Compute a base64-encoded hash of the nupkg at `path` using the algorithm
/// named by the NuGet feed (`SHA512`, `SHA256`, `MD5`). Chocolatey's
/// community feed records SHA512; support the other common values so the
/// check isn't brittle if the algorithm changes.
pub(super) fn compute_nupkg_hash(path: &std::path::Path, algorithm: &str) -> Result<String> {
    use base64::Engine as _;
    use sha2::Digest as _;

    let bytes = std::fs::read(path)
        .with_context(|| format!("chocolatey: read nupkg {}", path.display()))?;

    let digest: Vec<u8> = match algorithm.to_ascii_uppercase().as_str() {
        "SHA512" => sha2::Sha512::digest(&bytes).to_vec(),
        "SHA256" => sha2::Sha256::digest(&bytes).to_vec(),
        "MD5" => md5::Md5::digest(&bytes).to_vec(),
        other => anyhow::bail!(
            "chocolatey: unsupported feed hash algorithm '{}' (expected SHA512, SHA256, or MD5)",
            other
        ),
    };
    Ok(base64::engine::general_purpose::STANDARD.encode(&digest))
}

/// Push a .nupkg to a NuGet V2 API endpoint (Chocolatey, NuGet.org, etc.).
///
/// Matches the wire protocol used by the NuGet.Client library (what `choco
/// push` and `dotnet nuget push` use): PUT with multipart/form-data and a
/// NuGet-compatible User-Agent. The Chocolatey community repository's IIS
/// fronting rejects requests that don't match this shape with 403.
///
/// Retry policy comes from the user's top-level `retry:` block (defaults:
/// 10 attempts × 10s base × 5m cap — strictly more permissive than the
/// historical hardcoded 3-attempt loop). 5xx + 429 + transport errors retry
/// via [`retry_sync`]; 4xx fast-fails *except* the
/// Cloudflare/IIS "403/502/503/504 with HTML body" edge-challenge pattern,
/// which is forcibly retried by wrapping the failure in
/// [`anodizer_core::retry::Retriable`] so the classifier returns `true`
/// regardless of class. If the user wants the historical 3-attempt
/// behaviour they set `retry.attempts: 3` in their config.
pub(super) fn push_nupkg(
    nupkg_path: &std::path::Path,
    source: &str,
    api_key: &str,
    log: &StageLogger,
    policy: &RetryPolicy,
) -> Result<()> {
    use anodizer_core::retry::{HttpError, Retriable, retry_sync};
    use std::ops::ControlFlow;

    let filename = nupkg_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("package.nupkg")
        .to_string();
    let nupkg_data = std::fs::read(nupkg_path)
        .with_context(|| format!("chocolatey: read nupkg {}", nupkg_path.display()))?;

    // Normalize source URL and construct push endpoint.
    let base = source.trim_end_matches('/');
    let push_url = if base.ends_with("/api/v2/package") {
        base.to_string()
    } else if base.ends_with("/api/v2") {
        format!("{}/package", base)
    } else {
        format!("{}/api/v2/package", base)
    };

    log.status(&format!("pushing nupkg to {}", push_url));

    let client = reqwest::blocking::Client::builder()
        .user_agent("NuGet Command Line/6.10.0 (anodizer)")
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("chocolatey: build http client")?;

    // push.chocolatey.org is fronted by Cloudflare/IIS, which intermittently
    // returns 403 (and occasionally 503) with an HTML challenge body even for
    // valid NuGet PUTs. Standard 4xx fast-fail would mis-route those as
    // hard-fail; wrap them in `Retriable` so the classifier overrides the
    // default 4xx-Break behaviour. 5xx + 429 retry on their own via
    // HttpError-classification.
    retry_sync(policy, |attempt| {
        let form_file = match reqwest::blocking::multipart::Part::bytes(nupkg_data.clone())
            .file_name(filename.clone())
            .mime_str("application/octet-stream")
            .context("chocolatey: build multipart part")
        {
            Ok(p) => p,
            Err(e) => return Err(ControlFlow::Break(e)),
        };
        let form = reqwest::blocking::multipart::Form::new().part("package", form_file);

        let response = match client
            .put(&push_url)
            .header("X-NuGet-ApiKey", api_key)
            .header("X-NuGet-Client-Version", "6.10.0")
            .header("X-NuGet-Protocol-Version", "4.1.0")
            .multipart(form)
            .send()
        {
            Ok(r) => r,
            Err(e) => {
                // Transport-layer failure: unconditionally retry. Matches the
                // historical 3-attempt loop's behavior. The surrounding
                // retry_sync helper doesn't invoke is_retriable, so we own the
                // classification.
                let wrapped =
                    anyhow::Error::new(e).context(format!("chocolatey: push to {}", push_url));
                return Err(ControlFlow::Continue(wrapped));
            }
        };

        let status = response.status();
        if status.is_success() || status.as_u16() == 201 {
            return Ok(());
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let body = anodizer_core::http::body_of_blocking(response);
        let body_looks_html =
            content_type.contains("text/html") || body.trim_start().starts_with('<');

        // Only 502/503/504 are transient: the edge could not reach the NuGet
        // backend, and the identical request succeeds once it can. A 403 is NOT
        // transient — it is an authorization decision the registry already made,
        // and an automated push client cannot satisfy a Cloudflare 403 challenge
        // by retrying (it runs no JavaScript). Retrying a 403 only wastes minutes
        // and buries a real rejection behind an optimistic "edge challenge" guess.
        let gateway_transient = matches!(status.as_u16(), 502..=504) && body_looks_html;

        if gateway_transient {
            log.warn(&format!(
                "chocolatey gateway returned HTTP {} (attempt {}); retrying",
                status, attempt
            ));
            let base_err = anyhow::anyhow!(
                "chocolatey: push failed with HTTP {} to {} (attempt {}) — transient \
                 gateway error: {}",
                status,
                push_url,
                attempt,
                redact_bearer_tokens(&summarize_response_body(&body)),
            );
            let http_err =
                HttpError::new(std::io::Error::other(base_err.to_string()), status.as_u16());
            return Err(ControlFlow::Continue(anyhow::Error::new(Retriable::new(
                http_err,
            ))));
        }

        if status.as_u16() == 403 {
            // Surface what the registry actually said plus the concrete things to
            // check, in order. The causes are listed, not asserted: a generic
            // IIS/Cloudflare 403 body does not on its own disambiguate a bad key
            // from a permissions gap from a full moderation queue.
            return Err(ControlFlow::Break(anyhow::anyhow!(
                "chocolatey: push to {} was rejected with HTTP 403 Forbidden (not retried — \
                 403 is an authorization decision, not a transient error). Check, in order: \
                 (1) CHOCO_API_KEY is valid and unexpired, (2) the account may push this \
                 package id, (3) the package is not over its moderation-queue limit \
                 (community.chocolatey.org/packages/<id>). Registry response: {}",
                push_url,
                redact_bearer_tokens(&summarize_response_body(&body)),
            )));
        }

        let base_err = anyhow::anyhow!(
            "chocolatey: push failed with HTTP {} to {} (attempt {}): {}",
            status,
            push_url,
            attempt,
            redact_bearer_tokens(&summarize_response_body(&body)),
        );
        if anodizer_core::retry::status_is_retriable(status.as_u16()) {
            // 5xx / 429 retry naturally.
            Err(ControlFlow::Continue(base_err))
        } else {
            // Real 4xx (auth failure, malformed package, etc.) — fast-fail.
            Err(ControlFlow::Break(base_err))
        }
    })
}

/// Reduce an HTTP error-response body to the operator-facing essentials. IIS /
/// Cloudflare 403 pages wrap a single useful sentence in tens of lines of CSS
/// and markup; this pulls the `<title>` and heading text so the surfaced error
/// reads as a sentence. Non-HTML bodies are whitespace-collapsed and capped.
fn summarize_response_body(body: &str) -> String {
    const CAP: usize = 400;
    let trimmed = body.trim();
    let lower = trimmed.to_ascii_lowercase();
    let looks_html = trimmed.starts_with('<') || lower.contains("<html");
    if !looks_html {
        return cap_chars(&collapse_ws(trimmed), CAP);
    }
    let mut parts: Vec<String> = Vec::new();
    for tag in ["title", "h1", "h2", "h3"] {
        if let Some(text) = tag_inner_text(&lower, trimmed, tag) {
            let text = collapse_ws(&text);
            if !text.is_empty() && !parts.contains(&text) {
                parts.push(text);
            }
        }
    }
    let joined = if parts.is_empty() {
        collapse_ws(&strip_tags(trimmed))
    } else {
        parts.join(" — ")
    };
    cap_chars(&joined, CAP)
}

/// Inner text of the first `<tag …>…</tag>`. `lower` is `original` lowercased so
/// the tag search is case-insensitive; `to_ascii_lowercase` is byte-length
/// preserving, so the offsets index back into `original` to keep its casing.
fn tag_inner_text(lower: &str, original: &str, tag: &str) -> Option<String> {
    let open_at = lower.find(&format!("<{tag}"))?;
    let after_open = open_at + lower[open_at..].find('>')? + 1;
    let close_at = after_open + lower[after_open..].find(&format!("</{tag}>"))?;
    Some(original[after_open..close_at].to_string())
}

/// Best-effort tag stripping for the fallback summary — drop everything between
/// `<` and the next `>`. Not a real HTML parser; just enough to surface text.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::log::{StageLogger, Verbosity};

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 4,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(2),
        }
    }

    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    fn write_dummy_nupkg() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("foo.1.0.0.nupkg"), b"dummy nupkg bytes")
            .expect("write nupkg");
        dir
    }

    #[test]
    fn push_nupkg_retries_503_then_succeeds() {
        use std::sync::atomic::Ordering;

        let dir = write_dummy_nupkg();
        let path = dir.path().join("foo.1.0.0.nupkg");

        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
        ]);
        let source = format!("http://{addr}/api/v2/package");
        let log = StageLogger::new("test", Verbosity::Normal);

        push_nupkg(&path, &source, "apikey", &log, &fast_policy()).expect("retries 5xx then 201");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one 503 retry then 201 success"
        );
    }

    #[test]
    fn push_nupkg_403_with_html_body_fast_fails() {
        // A 403 is an authorization decision, not a transient edge challenge: it
        // must NOT retry (a push client cannot satisfy a JS challenge), and the
        // surfaced error must carry the registry's own message rather than an
        // optimistic "edge challenge" guess. Regression for the moderation /
        // credential 403 that previously force-retried 10× and burned the release.
        use std::sync::atomic::Ordering;

        let dir = write_dummy_nupkg();
        let path = dir.path().join("foo.1.0.0.nupkg");

        let html_body = "<html><head><title>403 - Forbidden: Access is denied.</title></head>\
                         <body><h2>403 - Forbidden: Access is denied.</h2>\
                         <h3>You do not have permission to view this directory or page using the \
                         credentials that you supplied.</h3></body></html>";
        let html_len = html_body.len();
        let html_resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: text/html\r\nContent-Length: {html_len}\r\n\r\n{html_body}"
            )
            .into_boxed_str(),
        );
        // A second 201 is queued; a correct implementation never reaches it.
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            html_resp,
            "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
        ]);
        let source = format!("http://{addr}/api/v2/package");
        let log = StageLogger::new("test", Verbosity::Normal);

        let err = push_nupkg(&path, &source, "apikey", &log, &fast_policy())
            .expect_err("403 must fast-fail, not retry");
        let msg = format!("{err:#}");
        assert!(msg.contains("403"), "error must mention 403: {msg}");
        assert!(
            msg.contains("credentials that you supplied") || msg.contains("Access is denied"),
            "error must surface the registry's own message: {msg}"
        );
        assert!(
            !msg.to_ascii_lowercase().contains("edge challenge"),
            "must not relabel a real 403 as a benign edge challenge: {msg}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "403 must NOT retry");
    }

    #[test]
    fn push_nupkg_retries_503_with_html_body() {
        // 502/503/504 stay transient: the edge could not reach the backend, and
        // the identical request succeeds once it can.
        use std::sync::atomic::Ordering;

        let dir = write_dummy_nupkg();
        let path = dir.path().join("foo.1.0.0.nupkg");

        let html_body = "<html><body>503 backend temporarily unavailable</body></html>";
        let html_len = html_body.len();
        let html_resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/html\r\nContent-Length: {html_len}\r\n\r\n{html_body}"
            )
            .into_boxed_str(),
        );
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            html_resp,
            "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
        ]);
        let source = format!("http://{addr}/api/v2/package");
        let log = StageLogger::new("test", Verbosity::Normal);

        push_nupkg(&path, &source, "apikey", &log, &fast_policy())
            .expect("503 gateway error retries to 201");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one 503 retry then 201");
    }

    #[test]
    fn summarize_response_body_extracts_iis_403_text() {
        let page = "<!DOCTYPE html><html><head><title>403 - Forbidden: Access is denied.</title>\
                    <style>body{color:red}</style></head><body><div id=\"header\"><h1>Server Error</h1></div>\
                    <h2>403 - Forbidden: Access is denied.</h2>\
                    <h3>You do not have permission to view this directory or page using the credentials that you supplied.</h3></body></html>";
        let s = summarize_response_body(page);
        assert!(s.contains("403 - Forbidden: Access is denied."), "got: {s}");
        assert!(s.contains("credentials that you supplied"), "got: {s}");
        assert!(
            !s.contains("<h2>") && !s.contains("color:red"),
            "markup/CSS leaked into summary: {s}"
        );
    }

    #[test]
    fn summarize_response_body_passes_through_plain_text() {
        assert_eq!(summarize_response_body("  bad apikey  "), "bad apikey");
    }

    #[test]
    fn push_nupkg_4xx_with_json_body_fast_fails() {
        // 401 Unauthorized with a JSON body is a real auth error — must not
        // retry. Contrast push_nupkg_retries_403_with_html_body.
        use std::sync::atomic::Ordering;

        let dir = write_dummy_nupkg();
        let path = dir.path().join("foo.1.0.0.nupkg");

        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 22\r\n\r\n{\"error\":\"bad apikey\"}",
        ]);
        let source = format!("http://{addr}/api/v2/package");
        let log = StageLogger::new("test", Verbosity::Normal);

        let err = push_nupkg(&path, &source, "apikey", &log, &fast_policy())
            .expect_err("401 must fast-fail");
        assert!(
            err.to_string().contains("401"),
            "error must mention 401: {err}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "4xx must NOT retry");
    }

    #[test]
    fn package_feed_hash_retries_5xx_then_returns_present() {
        // Use the user-supplied source as the query base (the
        // push.chocolatey.org → community.chocolatey.org remap only kicks
        // in when push_source contains 'push.chocolatey.org').
        use std::sync::atomic::Ordering;

        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<entry>
  <id>http://example.com/api/v2/Packages(Id='foo',Version='1.0.0')</id>
  <m:properties>
    <d:PackageHash>abc==</d:PackageHash>
    <d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>
  </m:properties>
</entry>"#;
        let body_len = body.len();
        let ok_resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {body_len}\r\n\r\n{body}"
            )
            .into_boxed_str(),
        );
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            ok_resp,
        ]);
        let source = format!("http://{addr}/api/v2/package");

        let result = package_feed_hash(&source, "foo", "1.0.0", &fast_policy());
        match result {
            FeedHashResult::Present {
                hash, algorithm, ..
            } => {
                assert_eq!(hash, "abc==");
                assert_eq!(algorithm, "SHA512");
            }
            other => panic!("expected Present, got: {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one 503 retry then 200");
    }

    /// Defense-in-depth: a Chocolatey gallery 4xx response that echoes our
    /// `Authorization: Bearer <PAT>` header back must not leak the token
    /// into the user-visible error chain. Exercises `push_nupkg`'s
    /// `base_err` formatter on the 4xx fast-fail path (the same wrap also
    /// guards `package_feed_hash` and the retry log lines).
    #[test]
    fn push_nupkg_redacts_bearer_in_error_body() {
        let dir = write_dummy_nupkg();
        let path = dir.path().join("foo.1.0.0.nupkg");

        let leaky = r#"{"error":"Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg"}"#;
        let body_len = leaky.len();
        let resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\n\r\n{leaky}"
            )
            .into_boxed_str(),
        );
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
        let source = format!("http://{addr}/api/v2/package");
        let log = StageLogger::new("test", Verbosity::Normal);

        let err = push_nupkg(&path, &source, "apikey", &log, &fast_policy())
            .expect_err("401 must fast-fail");
        let chain = format!("{err:#}");
        assert!(
            !chain.contains("ghp_FAKETOKEN1234567890abcdefg"),
            "bearer token leaked into error chain: {chain}"
        );
        assert!(
            chain.contains("<redacted>"),
            "expected `<redacted>` marker in error chain: {chain}"
        );
    }

    #[test]
    fn classify_moderation_submitted_and_unknown_are_in_moderation() {
        let (_, in_mod) = classify_moderation(Some("Submitted"), None);
        assert!(in_mod, "Submitted must be in_moderation=true");
        let (_, in_mod) = classify_moderation(Some("Unknown"), None);
        assert!(in_mod, "Unknown must be in_moderation=true");
    }

    #[test]
    fn classify_moderation_exempted_is_live_not_in_moderation() {
        // "Exempted" means the package is exempt from the moderation queue —
        // the version is immutable and live, not pending review.
        let (label, in_mod) = classify_moderation(Some("Exempted"), None);
        assert!(
            !in_mod,
            "Exempted must be in_moderation=false (live package): label={label}"
        );
        assert!(
            label.contains("exempted"),
            "label must mention exemption: {label}"
        );
    }

    #[test]
    fn classify_moderation_approved_is_live() {
        let (_, in_mod) = classify_moderation(Some("Approved"), None);
        assert!(!in_mod, "Approved must be in_moderation=false");
    }
}
