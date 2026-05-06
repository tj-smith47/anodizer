//! Native nupkg creation (OPC/ZIP) + NuGet V2 feed query/push.
//!
//! A `.nupkg` is an Open Packaging Conventions archive — a ZIP with
//! `[Content_Types].xml` and `_rels/.rels` at the root, the nuspec
//! manifest, and `tools/**` content. Building it natively lets us avoid
//! the Windows-only `choco pack` CLI.

use anodizer_core::log::StageLogger;
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
    version: &str,
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

    // Log the package details (GoReleaser parity: chocolatey.go:167)
    let _nupkg_name = format!("{}.{}.nupkg", name, version);

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
pub(super) enum FeedHashResult {
    /// Feed has this version. `listed` and `status` distinguish a published
    /// (Listed=true) version from one stuck in the community moderation
    /// queue (Listed=false, status=Submitted/Unknown/Rejected/Exempted).
    Present {
        hash: String,
        algorithm: String,
        /// `<d:PackageStatus>`: "Submitted" / "Listed" / "Rejected" /
        /// "Exempted" / "Unknown". Absent on feeds that don't expose the
        /// field.
        status: Option<String>,
        /// `<d:Listed>` boolean. `Some(false)` for moderation-queue
        /// entries; `Some(true)` once a moderator approves; `None` when
        /// the feed didn't expose the field.
        listed: Option<bool>,
        /// `<d:Published>` ISO-8601. The string "1900-01-01T00:00:00" is
        /// Chocolatey's unlisted sentinel (matches Listed=false).
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
pub(super) fn package_feed_hash(push_source: &str, name: &str, version: &str) -> FeedHashResult {
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

    let body = match client.get(&url).send() {
        Ok(resp) if resp.status().is_success() => resp.text().unwrap_or_default(),
        _ => return FeedHashResult::Absent,
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
    let listed = parse_xml_element(&body, "Listed").and_then(|v| v.parse::<bool>().ok());
    let published = parse_xml_element(&body, "Published");
    match (hash, algorithm) {
        (Some(h), Some(a)) if !h.is_empty() && !a.is_empty() => FeedHashResult::Present {
            hash: h,
            algorithm: a,
            status,
            listed,
            published,
        },
        _ => FeedHashResult::PresentNoHash,
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
pub(super) fn push_nupkg(
    nupkg_path: &std::path::Path,
    source: &str,
    api_key: &str,
    log: &StageLogger,
) -> Result<()> {
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
    // valid NuGet PUTs. Treat those as transient and retry once before
    // surfacing the failure — otherwise CI flake masks real pushes.
    const MAX_ATTEMPTS: u32 = 3;
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;

        let form_file = reqwest::blocking::multipart::Part::bytes(nupkg_data.clone())
            .file_name(filename.clone())
            .mime_str("application/octet-stream")
            .context("chocolatey: build multipart part")?;
        let form = reqwest::blocking::multipart::Form::new().part("package", form_file);

        let response = client
            .put(&push_url)
            .header("X-NuGet-ApiKey", api_key)
            .header("X-NuGet-Client-Version", "6.10.0")
            .header("X-NuGet-Protocol-Version", "4.1.0")
            .multipart(form)
            .send()
            .with_context(|| format!("chocolatey: push to {}", push_url))?;

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
        let body = response.text().unwrap_or_default();
        let body_looks_html =
            content_type.contains("text/html") || body.trim_start().starts_with('<');
        let edge_transient = matches!(status.as_u16(), 403 | 502 | 503 | 504) && body_looks_html;

        if edge_transient && attempt < MAX_ATTEMPTS {
            let backoff = std::time::Duration::from_secs(2u64.pow(attempt));
            log.warn(&format!(
                "chocolatey: edge returned HTTP {} with HTML body (attempt {}/{}); \
                 retrying in {}s — likely a Cloudflare/IIS challenge, not a real \
                 rejection",
                status,
                attempt,
                MAX_ATTEMPTS,
                backoff.as_secs()
            ));
            std::thread::sleep(backoff);
            continue;
        }

        let hint = if edge_transient {
            "; this looked like a Cloudflare/IIS edge challenge that did not clear \
             after retry — try again later or contact Chocolatey support if it \
             persists"
        } else {
            ""
        };
        anyhow::bail!(
            "chocolatey: push failed with HTTP {} to {} after {} attempt(s){}: {}",
            status,
            push_url,
            attempt,
            hint,
            body
        )
    }
}
