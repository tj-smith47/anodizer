use anodizer_core::artifact::ArtifactKind;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryPolicy, SuccessClass, retry_http_blocking};
use anyhow::{Context as _, Result, anyhow, bail};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Returns the default formats for CloudSmith uploads: apk, deb, rpm.
pub fn cloudsmith_default_formats() -> Vec<&'static str> {
    crate::util::default_package_formats()
}

/// Check if a filename matches any of the given format extensions.
///
/// The user-facing CloudSmith config (per Pro docs) uses `apk`, `deb`,
/// `rpm`, `src.rpm` as filter slugs. CloudSmith's API path slug for
/// `.apk` files is `alpine`, so users may write either spelling — both
/// are recognized here. `srpm` / `src.rpm` strip the dotted prefix when
/// matched against a `.src.rpm` filename (the dotted slug otherwise
/// won't match through the generic suffix helper).
pub fn cloudsmith_format_matches(filename: &str, formats: &[impl AsRef<str>]) -> bool {
    let lower = filename.to_ascii_lowercase();
    for fmt in formats {
        let raw = fmt.as_ref();
        let suffix = match raw {
            "alpine" => ".apk",
            "srpm" | "src.rpm" => ".src.rpm",
            // Non-aliased slug: defer to the shared case-folding matcher so
            // a mixed-case slug (e.g. `DEB`) still matches a `.deb` file.
            other => {
                if crate::util::format_matches(&lower, &[other]) {
                    return true;
                }
                continue;
            }
        };
        if lower.ends_with(suffix) {
            return true;
        }
    }
    false
}

/// Cloudsmith API base URL (used for files/create and packages/upload/*).
const CLOUDSMITH_API_BASE: &str = "https://api.cloudsmith.io/v1";

/// Resolve the Cloudsmith API base URL. Defaults to [`CLOUDSMITH_API_BASE`];
/// `ANODIZE_CLOUDSMITH_API_BASE` overrides it so tests can point the 3-step
/// upload flow at a local responder without a real network call. The env
/// read is the only test seam — production runs never set the variable.
fn cloudsmith_api_base() -> String {
    std::env::var("ANODIZE_CLOUDSMITH_API_BASE").unwrap_or_else(|_| CLOUDSMITH_API_BASE.to_string())
}

/// Build the CloudSmith upload URL for the given org, repo, format, and distribution.
///
/// Retained for dry-run logging parity with prior versions. The live code
/// path uses the canonical 3-step API flow (files/create → S3 presigned
/// upload → packages/upload/{format}/) rather than this URL directly.
pub fn cloudsmith_upload_url(org: &str, repo: &str, format: &str, distribution: &str) -> String {
    format!(
        "{}/packages/{}/{}/upload/{}/ (distribution={})",
        CLOUDSMITH_API_BASE, org, repo, format, distribution
    )
}

/// Detect the package format from a filename extension.
///
/// Returns the CloudSmith API-side format slug (`alpine`, `deb`, `rpm`,
/// `srpm`, or `raw`). `.src.rpm` is matched BEFORE `.rpm` because the
/// suffix overlaps — CloudSmith treats source RPMs as a distinct format
/// at `/packages/<org>/<repo>/upload/srpm/`.
fn detect_format(filename: &str) -> &str {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".src.rpm") {
        "srpm"
    } else if lower.ends_with(".deb") {
        "deb"
    } else if lower.ends_with(".rpm") {
        "rpm"
    } else if lower.ends_with(".apk") {
        "alpine"
    } else {
        "raw"
    }
}

/// CloudSmith API format slugs that accept a Debian `component:` field.
/// Other formats silently ignore `component`; the upload code drops it
/// to avoid noise in the request body.
const COMPONENT_BEARING_FORMATS: &[&str] = &["deb"];

/// The accept-all distribution slug CloudSmith requires for a `format` when
/// the user configured no `distributions.<format>` entry, or `None` for
/// formats that don't require a distribution.
///
/// CloudSmith's `.deb` and `alpine`/`.apk` uploads MUST carry a
/// `distribution`; omitting it leaves the package accepted-but-unindexed and
/// thus not `apt`/`apk` installable. Per CloudSmith's docs the catch-all
/// values are `any-distro/any-version` (deb) and `alpine/any-version`
/// (alpine), which keep the package installable across distro versions while
/// still letting a user pin a real distro (`debian/bookworm`) via config.
/// `rpm`/`srpm`/`raw` do not require a distribution, so they return `None`
/// and continue to upload with the key omitted.
fn cloudsmith_default_distribution(format: &str) -> Option<&'static str> {
    match format {
        "deb" => Some("any-distro/any-version"),
        "alpine" => Some("alpine/any-version"),
        _ => None,
    }
}

/// Outcome of checking whether a package already exists on Cloudsmith.
/// Returned by [`check_cloudsmith_package_exists`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CloudsmithPackageState {
    /// No package found with the given filename: caller should upload.
    NotFound,
    /// Package found with matching md5: caller should skip (idempotent).
    SkipIdempotent,
    /// Package found with a different md5: caller should bail loudly.
    /// `remote` is the md5 reported by Cloudsmith.
    Md5Mismatch { remote: String },
}

/// Classify a Cloudsmith packages-list response body against the local md5.
///
/// Pure function so the decision rule can be unit-tested without I/O.
/// Cloudsmith returns a JSON array of package objects; each entry has at
/// least `filename` and `checksum_md5`. We look for the first entry whose
/// `filename` matches `art_name` exactly.
///
/// Field names verified against the live Cloudsmith OpenAPI spec at
/// `https://api.cloudsmith.io/openapi/` — `Package` definition:
///
/// - `filename`: string (title "Filename")
/// - `checksum_md5`: string, readOnly
///
/// The packages_list endpoint (`GET /packages/{owner}/{repo}/`) returns
/// `type: array, items: $ref '#/definitions/Package'` — no envelope.
pub(crate) fn classify_cloudsmith_package_response(
    body: &str,
    art_name: &str,
    local_md5: &str,
) -> Result<CloudsmithPackageState> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .with_context(|| format!("cloudsmith: parse packages-list body: {}", body.trim()))?;
    let array = match parsed.as_array() {
        Some(a) => a,
        None => return Ok(CloudsmithPackageState::NotFound),
    };
    for entry in array {
        let filename = entry.get("filename").and_then(|v| v.as_str()).unwrap_or("");
        if filename != art_name {
            continue;
        }
        let remote_md5 = entry
            .get("checksum_md5")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if remote_md5.is_empty() {
            // Package exists but Cloudsmith didn't report a checksum we can
            // verify. Treat as idempotent skip rather than upload-and-create
            // a duplicate: presence-by-filename is the strongest signal we have.
            return Ok(CloudsmithPackageState::SkipIdempotent);
        }
        if remote_md5 == local_md5.to_ascii_lowercase() {
            return Ok(CloudsmithPackageState::SkipIdempotent);
        }
        return Ok(CloudsmithPackageState::Md5Mismatch { remote: remote_md5 });
    }
    Ok(CloudsmithPackageState::NotFound)
}

// ---------------------------------------------------------------------------
// keep_versions pruning — pure selection
// ---------------------------------------------------------------------------

/// One CloudSmith package entry, as projected from a packages-list response,
/// for `keep_versions` retention ranking. Each `(slug, version)` pair is a
/// single uploaded artifact (one format/arch); many entries share a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloudsmithVersionEntry {
    /// Per-package permanent identifier (`slug_perm`, falling back to `slug`)
    /// used as the DELETE target.
    pub slug: String,
    /// The CloudSmith `version` string for this artifact, e.g. `0.9.1`,
    /// `1:0.9.1-1` (deb epoch + revision), or `0.9.1-r1` (apk revision).
    pub version: String,
    /// `uploaded_at` timestamp (RFC-3339); used as a tiebreak/fallback when a
    /// version string won't parse as SemVer. Empty when absent.
    pub uploaded_at: String,
}

/// Normalize a CloudSmith package `version` string to its base SemVer for
/// grouping all formats of one release together.
///
/// CloudSmith carries packaging-specific decorations that differ per format
/// for the same logical release:
/// - deb/rpm epoch prefix: `1:0.9.1-1` → strip `N:` and the `-1` revision
/// - apk revision suffix: `0.9.1-r1` → strip the `-rN` revision
///
/// Returns the bare `major.minor.patch[-prerelease]` so `0.9.1`, `1:0.9.1-1`,
/// and `0.9.1-r1` all collapse to `0.9.1`. A SemVer prerelease (`0.9.1-rc.1`)
/// is preserved (only the deb `-<digits>` / apk `-r<digits>` revision tails
/// are stripped).
pub(crate) fn normalize_cloudsmith_version(version: &str) -> String {
    // Strip a leading `epoch:` (deb/rpm) — digits before the first colon.
    let after_epoch = match version.split_once(':') {
        Some((epoch, rest)) if !epoch.is_empty() && epoch.bytes().all(|b| b.is_ascii_digit()) => {
            rest
        }
        _ => version,
    };
    // Strip a trailing packaging revision: apk `-rN` or deb `-N` where the
    // tail after the final `-` is `r?<digits>` AND the head is itself a bare
    // `major.minor.patch` SemVer. Gating on a SemVer head prevents truncating
    // a legitimate prerelease that merely looks revision-shaped (`1.0.0-r1`,
    // `1.0.0-rc-1`): those have a non-bare-SemVer head once the tail is split,
    // so they survive intact.
    if let Some((head, tail)) = after_epoch.rsplit_once('-') {
        let revision_body = tail.strip_prefix('r').unwrap_or(tail);
        let is_revision =
            !revision_body.is_empty() && revision_body.bytes().all(|b| b.is_ascii_digit());
        if is_revision && anodizer_core::git::parse_semver(head).is_ok() {
            return head.to_string();
        }
    }
    after_epoch.to_string()
}

/// Rank the distinct normalized versions present in `entries` newest-first,
/// returning `(ordered_versions, buckets)` where `buckets` maps each
/// normalized version to its member entries.
///
/// Ordering: by parsed SemVer descending when both compare; a parseable
/// version always outranks an unparseable one (garbage versions sink to the
/// bottom); two unparseable versions fall back to newest `uploaded_at` first.
/// Both [`select_versions_to_prune`] and the operator summary share this one
/// comparator so the "kept …" line can never disagree with what was deleted.
fn rank_distinct_versions_desc(
    entries: &[CloudsmithVersionEntry],
) -> (Vec<String>, HashMap<String, Vec<&CloudsmithVersionEntry>>) {
    let mut order: Vec<String> = Vec::new();
    let mut buckets: HashMap<String, Vec<&CloudsmithVersionEntry>> = HashMap::new();
    let mut newest_ts: HashMap<String, String> = HashMap::new();
    for e in entries {
        let norm = normalize_cloudsmith_version(&e.version);
        if !buckets.contains_key(&norm) {
            order.push(norm.clone());
        }
        let ts = newest_ts.entry(norm.clone()).or_default();
        if e.uploaded_at > *ts {
            *ts = e.uploaded_at.clone();
        }
        buckets.entry(norm).or_default().push(e);
    }
    order.sort_by(|a, b| {
        use std::cmp::Ordering;
        let sa = anodizer_core::git::parse_semver(a).ok();
        let sb = anodizer_core::git::parse_semver(b).ok();
        match (sa, sb) {
            (Some(va), Some(vb)) => vb.cmp(&va), // descending semver
            (Some(_), None) => Ordering::Less,   // parseable ranks first
            (None, Some(_)) => Ordering::Greater,
            (None, None) => {
                let ta = newest_ts.get(a).map(String::as_str).unwrap_or("");
                let tb = newest_ts.get(b).map(String::as_str).unwrap_or("");
                tb.cmp(ta) // newest uploaded first
            }
        }
    });
    (order, buckets)
}

/// Decide which package slugs to DELETE so that only the `keep` most-recent
/// distinct release versions remain — the heart of `cloudsmiths[].keep_versions`.
///
/// Pure (no I/O) so the destructive decision is unit-testable in isolation:
///
/// 1. Group every entry by its [`normalize_cloudsmith_version`] (so all
///    formats/arches of one release share a bucket).
/// 2. Rank the distinct normalized versions newest-first (see
///    [`rank_distinct_versions_desc`]).
/// 3. Keep the top `keep` versions; return the slugs of every entry whose
///    version ranks beyond `keep`.
///
/// `current_version` is the just-published release: its normalized form is
/// **always** kept regardless of ranking, so a ranking quirk can never delete
/// the artifacts this run just uploaded. An EMPTY `current_version` normalizes
/// to `""`, which matches no real bucket and so silently drops that
/// safety-net; the I/O caller therefore refuses to prune when the version is
/// unknown rather than relying on this function to notice. `keep == 0` returns
/// an empty vec (refuses to prune everything; the caller rejects `0` earlier,
/// this is a belt-and-braces guard).
pub(crate) fn select_versions_to_prune(
    entries: &[CloudsmithVersionEntry],
    keep: u32,
    current_version: &str,
) -> Vec<String> {
    if keep == 0 || entries.is_empty() {
        return Vec::new();
    }
    let current_norm = normalize_cloudsmith_version(current_version);
    let (order, buckets) = rank_distinct_versions_desc(entries);

    // Keep the top `keep`, plus the current version wherever it ranks.
    let keep = keep as usize;
    let mut kept: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for v in order.iter().take(keep) {
        kept.insert(v.as_str());
    }
    kept.insert(current_norm.as_str());

    let mut to_delete: Vec<String> = Vec::new();
    for v in &order {
        if kept.contains(v.as_str()) {
            continue;
        }
        if let Some(entries) = buckets.get(v) {
            for e in entries {
                to_delete.push(e.slug.clone());
            }
        }
    }
    to_delete
}

/// GET the Cloudsmith packages-list endpoint filtered by filename and
/// classify the result. Retries 5xx/429/transport via the shared retry
/// helper; 4xx fast-fails.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_cloudsmith_package_exists(
    client: &reqwest::blocking::Client,
    list_url: &str,
    query: &str,
    token: &str,
    art_name: &str,
    local_md5: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<CloudsmithPackageState> {
    log.verbose(&format!(
        "checking existing cloudsmith package for '{}' (query={})",
        art_name, query
    ));
    let result = retry_request("packages/list", art_name, policy, log, || {
        client
            .get(list_url)
            .query(&[("query", query), ("page_size", "100")])
            .header("Authorization", format!("token {}", token))
            .header("Accept", "application/json")
            .send()
    });
    let (_status, body) = match result {
        Ok(pair) => pair,
        Err(err) => {
            // Treat any failure to query as "unknown" — fall through to
            // upload rather than spuriously bail. The error has already been
            // shaped (and any bearer tokens redacted) by retry_request.
            log.warn(&format!(
                "could not query existing cloudsmith packages for '{}' ({}); attempting upload anyway",
                art_name, err
            ));
            return Ok(CloudsmithPackageState::NotFound);
        }
    };
    classify_cloudsmith_package_response(&body, art_name, local_md5)
}

/// Retry an HTTP request builder, threading classification through the
/// shared [`retry_http_blocking`] helper. `build_send` is called per attempt
/// so multipart bodies can be rebuilt. 5xx/429 + transport errors retry;
/// 4xx fast-fails. Returns `(status, body)` on success.
fn retry_request<F>(
    label: &str,
    art_name: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
    mut build_send: F,
) -> Result<(reqwest::StatusCode, String)>
where
    F: FnMut() -> Result<reqwest::blocking::Response, reqwest::Error>,
{
    let scope = format!("cloudsmith {label} for '{art_name}'");
    retry_http_blocking(
        &scope,
        policy,
        SuccessClass::Strict,
        |attempt| {
            if attempt > 1 {
                log.verbose(&format!(
                    "retrying cloudsmith {label} for '{art_name}' (attempt {attempt})"
                ));
            }
            build_send()
        },
        |status, body| {
            format!(
                "cloudsmith {label} for '{art_name}' returned HTTP {status}: {}",
                redact_bearer_tokens(body.trim())
            )
        },
    )
}

/// Stage a file for upload: request a `files/create` slot (step 1) and push
/// the bytes to the returned S3 presigned URL (step 2). Returns the
/// single-use `identifier` the caller passes to `packages/upload` (step 3).
///
/// A Cloudsmith files/create slot is consumed by exactly one package-create,
/// so a caller uploading to N distributions must call this once per
/// distribution to obtain N distinct identifiers.
#[allow(clippy::too_many_arguments)]
fn stage_cloudsmith_file(
    client: &reqwest::blocking::Client,
    api_base: &str,
    organization: &str,
    repository: &str,
    art_name: &str,
    md5_hex: &str,
    file_bytes: &[u8],
    token: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<String> {
    // --- Step 1/3: request a files/create slot ---
    //
    // POST /v1/files/{org}/{repo}/ with the filename + md5 returns a
    // short-lived S3 presigned upload URL plus the fields the upload POST
    // must include. This matches what the official Cloudsmith CLI's
    // `request_file_upload` helper does.
    let files_create_url = format!("{}/files/{}/{}/", api_base, organization, repository);
    let files_create_body = serde_json::json!({
        "filename": art_name,
        "md5_checksum": md5_hex,
        "method": "post",
    });

    log.verbose(&format!("POST {} (step 1 of 3)", files_create_url));
    let (_create_status, create_body) =
        retry_request("files/create", art_name, policy, log, || {
            client
                .post(&files_create_url)
                .header("Authorization", format!("token {}", token))
                .header("Accept", "application/json")
                .json(&files_create_body)
                .send()
        })?;
    let create_json: serde_json::Value = serde_json::from_str(&create_body).with_context(|| {
        format!(
            "cloudsmith files/create for '{}' returned non-JSON body: {}",
            art_name,
            create_body.trim()
        )
    })?;
    let identifier = create_json
        .get("identifier")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cloudsmith files/create response missing 'identifier' for '{}': {}",
                art_name,
                create_body.trim()
            )
        })?
        .to_string();
    let presigned_url = create_json
        .get("upload_url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cloudsmith files/create response missing 'upload_url' for '{}'",
                art_name
            )
        })?
        .to_string();
    let upload_fields = create_json
        .get("upload_fields")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    // --- Step 2/3: upload bytes to the presigned S3 URL ---
    //
    // The presigned URL is AWS S3 POST form — no Cloudsmith auth header is
    // added here. The fields returned in step 1 (policy, signature, key, ...)
    // MUST be included as multipart form text parts exactly as given, and the
    // actual file goes under the `file` key (not `package_file`).
    log.verbose(&format!("POST {} (presigned, step 2 of 3)", presigned_url));
    // Multipart Form is move-only, so we rebuild it on every retry attempt.
    // Cloning `file_bytes` and `upload_fields` per-attempt is the price of
    // retriability; the bytes are already in memory.
    let _ = retry_request("presigned upload", art_name, policy, log, || {
        let mut form = reqwest::blocking::multipart::Form::new();
        for (k, v) in &upload_fields {
            let val = v
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string());
            form = form.text(k.clone(), val);
        }
        let file_part = match reqwest::blocking::multipart::Part::bytes(file_bytes.to_vec())
            .file_name(art_name.to_string())
            .mime_str("application/octet-stream")
        {
            Ok(p) => p,
            // `mime_str` only fails on unparsable MIME; the literal
            // `"application/octet-stream"` is hard-coded and a valid RFC-2045
            // token, so this arm is structurally unreachable.
            Err(_) => unreachable!("application/octet-stream is a valid MIME type"),
        };
        form = form.part("file", file_part);
        client.post(&presigned_url).multipart(form).send()
    })?;

    Ok(identifier)
}

// ---------------------------------------------------------------------------
// publish_to_cloudsmith
// ---------------------------------------------------------------------------

/// Format the single default-verbosity summary line for one cloudsmith entry,
/// collapsing the per-file `uploading …` / `uploaded …` / `skipping …`
/// firehose into one line. `uploaded` counts artifacts this run newly landed;
/// `skipped` counts artifacts already present with a matching md5 (no upload
/// issued).
pub(crate) fn cloudsmith_upload_summary(
    uploaded: usize,
    skipped: usize,
    org: &str,
    repo: &str,
) -> String {
    format!("uploaded {uploaded} artifact(s), skipped {skipped} (already present) → {org}/{repo}")
}

/// Upload packages to CloudSmith via the CloudSmith API.
///
/// This is a top-level publisher: it reads from `ctx.config.cloudsmiths` rather
/// than from per-crate publish configs.  Each entry specifies an organization,
/// repository, optional credential env var, and optional format/distribution
/// filters.
///
/// Returns the list of [`CloudsmithTarget`]s actually uploaded this run, with
/// the `slug` (Cloudsmith's per-package permanent identifier) populated when
/// the step-3 `packages/upload/<format>/` response surfaced one. The returned
/// list drives `PublishEvidence::extra.cloudsmith_targets` so [`rollback`]
/// can issue real `DELETE /v1/packages/<org>/<repo>/<slug>/` calls; targets
/// whose slug couldn't be parsed degrade to the warn-only manual-cleanup
/// path (see [`cloudsmith_manual_cleanup_msg`]).
///
/// SkipIdempotent matches (artifact already present with matching md5) are
/// NOT included in the return — rollback's semantic is "undo what this run
/// uploaded," and a remote-side hit was put there by an earlier run.
pub(crate) fn publish_to_cloudsmith(
    ctx: &Context,
    log: &StageLogger,
) -> Result<Vec<CloudsmithTarget>> {
    let mut uploaded: Vec<CloudsmithTarget> = Vec::new();
    let entries = match ctx.config.cloudsmiths {
        Some(ref v) if !v.is_empty() => v,
        _ => return Ok(uploaded),
    };

    // Single retry policy resolved from the top-level `retry:` block; reused
    // for every step of the 3-stage upload (files/create → S3 presigned →
    // packages/upload). The retry policy is set
    // once per pipe invocation.
    let policy = ctx.retry_policy();

    for entry in entries {
        // Check skip flag.
        if let Some(ref s) = entry.skip {
            let off = s
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .with_context(|| "cloudsmith: render skip template")?;
            if off {
                log.status("skipped cloudsmith entry — skip evaluates true");
                continue;
            }
        }

        let proceed = anodizer_core::config::evaluate_if_condition(
            entry.if_condition.as_deref(),
            "cloudsmith entry",
            |t| ctx.render_template(t),
        )?;
        if !proceed {
            log.status("skipped cloudsmith entry — `if` condition evaluated falsy");
            continue;
        }

        // Organization is required — bail before dry-run so config errors
        // surface even in dry-run mode.
        let org_raw = match entry.organization.as_deref() {
            Some(o) if !o.is_empty() => o,
            _ => bail!("cloudsmith: 'organization' is required but not set"),
        };

        // Repository is required.
        let repo_raw = match entry.repository.as_deref() {
            Some(r) if !r.is_empty() => r,
            _ => bail!("cloudsmith: 'repository' is required but not set"),
        };

        // Render organization and repository through template engine in case
        // they contain template expressions.
        let organization = ctx
            .render_template(org_raw)
            .with_context(|| format!("cloudsmith: failed to render organization '{}'", org_raw))?;

        let repository = ctx
            .render_template(repo_raw)
            .with_context(|| format!("cloudsmith: failed to render repository '{}'", repo_raw))?;

        // Resolve the secret env-var name (default: CLOUDSMITH_TOKEN).
        let secret_name_rendered =
            crate::util::resolve_secret_name(ctx, entry.secret_name.as_deref(), "CLOUDSMITH_TOKEN");

        // Determine formats filter.
        let formats: Vec<String> = match entry.formats {
            Some(ref f) if !f.is_empty() => f.clone(),
            _ => cloudsmith_default_formats()
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };

        // Resolve distributions map (format -> Vec<distro string>). Each
        // entry yields one or more distribution slugs (the publisher
        // issues one upload per slug). A
        // template-rendering failure on any slug is a config error and
        // hard-bails so a typo doesn't silently route an upload to the
        // wrong distribution.
        let distributions: HashMap<String, Vec<String>> = match entry.distributions {
            Some(ref d) => {
                let mut out: HashMap<String, Vec<String>> = HashMap::new();
                for (k, v) in d {
                    let raw_entries = v.to_str_vec();
                    let mut rendered_entries: Vec<String> = Vec::with_capacity(raw_entries.len());
                    for raw in raw_entries {
                        let rendered = ctx.render_template(raw).with_context(|| {
                            format!(
                                "cloudsmith: render distribution slug '{}' for format '{}'",
                                raw, k
                            )
                        })?;
                        rendered_entries.push(rendered);
                    }
                    out.insert(k.clone(), rendered_entries);
                }
                out
            }
            None => HashMap::new(),
        };

        // Resolve component (optional, used for deb).
        let component = entry
            .component
            .as_ref()
            .map(|c| crate::util::render_or_warn(ctx, log, "cloudsmith.component", c))
            .transpose()?;

        // Check republish flag.
        let republish = match entry.republish.as_ref() {
            Some(r) => r
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .with_context(|| "cloudsmith: render republish template")?,
            None => false,
        };

        // Collect matching artifacts.
        let artifacts: Vec<_> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| {
                let valid_kind =
                    matches!(a.kind, ArtifactKind::LinuxPackage | ArtifactKind::Archive);
                if !valid_kind {
                    return false;
                }
                if !cloudsmith_format_matches(a.name(), &formats) {
                    return false;
                }
                crate::util::matches_id_filter(a, entry.ids.as_deref())
            })
            .collect();

        // --- Dry-run logging ---
        if ctx.is_dry_run() {
            let sample_url =
                cloudsmith_upload_url(&organization, &repository, "{format}", "{distribution}");
            log.status(&format!(
                "(dry-run) would upload packages to CloudSmith org '{}' repo '{}' at {}",
                organization, repository, sample_url
            ));
            log.status(&format!("(dry-run) would filter to formats {:?}", formats));
            if let Some(ref ids) = entry.ids {
                log.status(&format!("(dry-run) would filter to build IDs {:?}", ids));
            }
            if !distributions.is_empty() {
                log.status(&format!(
                    "(dry-run) would publish to distributions {:?}",
                    distributions
                ));
            }
            if let Some(ref comp) = component {
                log.status(&format!("(dry-run) would use component {}", comp));
            }
            if republish {
                log.status("(dry-run) would republish existing versions");
            }
            log.status(&format!(
                "(dry-run) would read credentials from {}",
                secret_name_rendered
            ));
            log.status(&format!("(dry-run) {} artifacts matched", artifacts.len()));
            for a in &artifacts {
                log.status(&format!("(dry-run)   {} ({})", a.name(), a.kind));
            }
            continue;
        }

        // --- Live mode ---
        // Resolve token from environment.
        let token = ctx.env_var(&secret_name_rendered).ok_or_else(|| {
            anyhow!(
                "cloudsmith: environment variable '{}' not set (needed for org '{}' repo '{}')",
                secret_name_rendered,
                organization,
                repository
            )
        })?;

        if artifacts.is_empty() {
            log.status(&format!(
                "no matching cloudsmith artifacts for org '{}' repo '{}' (formats: {:?})",
                organization, repository, formats
            ));
            continue;
        }

        let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(60))
            .context("cloudsmith: failed to build HTTP client")?;

        log.status(&format!(
            "uploading {} packages to cloudsmith org '{}' repo '{}'",
            artifacts.len(),
            organization,
            repository
        ));

        // Distinct CloudSmith package names uploaded under this entry, used to
        // scope post-upload `keep_versions` pruning to each package alone.
        let mut prune_package_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        // Per-entry tallies for the single default-verbosity summary line; the
        // per-file upload/skip detail below is verbose-only. `uploaded_count`
        // increments per landed package-create (per distro slug, matching the
        // verbose `uploaded …` lines); `skipped_count` per already-present
        // idempotent skip.
        let mut uploaded_count = 0usize;
        let mut skipped_count = 0usize;

        for artifact in &artifacts {
            let path = &artifact.path;
            if !path.exists() {
                bail!("cloudsmith: artifact file not found: {}", path.display());
            }

            let art_name = artifact.name();
            let fmt = detect_format(art_name);

            // Look up distribution(s) for this format. Cloudsmith accepts an
            // `any-distro/any-version` pseudo-entry for repos that aren't
            // distro-pinned, so an empty list is valid input and treated as
            // "no distribution override". The array form
            // produces one upload per slug.
            //
            // Routing is keyed on the API-side format slug (`apk`/`alpine`,
            // `deb`, `rpm`, `srpm`). The user-facing config key may be
            // either spelling — handle both so a config written against
            // the docs (which use `apk`) and one written against
            // CloudSmith's API path (`alpine`) both work.
            let distro_slugs: Vec<String> = {
                let mut slugs: Vec<String> = distributions.get(fmt).cloned().unwrap_or_default();
                if slugs.is_empty() && fmt == "alpine" {
                    slugs = distributions.get("apk").cloned().unwrap_or_default();
                }
                if slugs.is_empty() && fmt == "srpm" {
                    slugs = distributions.get("src.rpm").cloned().unwrap_or_default();
                }
                slugs
            };

            let file_bytes = std::fs::read(path)
                .with_context(|| format!("cloudsmith: failed to read '{}'", path.display()))?;
            let size_bytes = file_bytes.len();

            // Cloudsmith's files/create API wants a hex-lowercase md5 of
            // the raw bytes.
            let md5_hex = {
                use md5::Digest as _;
                let mut hasher = md5::Md5::new();
                hasher.update(&file_bytes);
                anodizer_core::hashing::hex_lower(&hasher.finalize())
            };

            // Pre-check (republish=false only): query Cloudsmith for an
            // existing package with this filename. If found and md5
            // matches, skip (idempotent). If found but md5 differs,
            // bail — we can't fix the mismatch (the package is immutable
            // on Cloudsmith's side) and silently re-uploading produces
            // duplicate packages with different hashes.
            //
            // The `check_url` / `query` are built unconditionally so the
            // step-3 409-recovery path below can re-issue the same query
            // when an upload races against another concurrent CI loop
            // submitting the same package between pre-check and step-3.
            let api_base = cloudsmith_api_base();
            let check_url = format!("{}/packages/{}/{}/", api_base, organization, repository);
            let check_query = format!("filename:{}", art_name);
            if !republish {
                match check_cloudsmith_package_exists(
                    &client,
                    &check_url,
                    &check_query,
                    &token,
                    art_name,
                    &md5_hex,
                    &policy,
                    log,
                )? {
                    CloudsmithPackageState::SkipIdempotent => {
                        // Per-file skip detail is verbose-only; the entry
                        // summary reports the aggregate skip count.
                        log.verbose(&format!(
                            "skipped '{}' — already uploaded with matching md5",
                            art_name
                        ));
                        skipped_count += 1;
                        continue;
                    }
                    CloudsmithPackageState::Md5Mismatch { remote } => {
                        bail!(
                            "cloudsmith: '{}' already exists in org '{}' repo '{}' \
                             with a different md5 (remote={}, local={}). \
                             Re-uploading would create a conflicting duplicate. \
                             Set republish: true to force overwrite.",
                            art_name,
                            organization,
                            repository,
                            remote,
                            md5_hex
                        );
                    }
                    CloudsmithPackageState::NotFound => {}
                }
            }

            // Iterate at least once even when no distributions are
            // configured. For formats CloudSmith requires a distribution on
            // (`deb`, `alpine`), fall back to the accept-all catch-all slug so
            // the package still indexes and stays installable; an empty slug
            // for those would land the bytes unindexed. Formats that don't
            // require a distribution (`rpm`/`srpm`/`raw`) keep the
            // empty-slug "no override" behaviour.
            let upload_slugs: Vec<String> = if distro_slugs.is_empty() {
                match cloudsmith_default_distribution(fmt) {
                    Some(default_distro) => {
                        log.verbose(&format!(
                            "no distribution configured for '{}' ({}); defaulting to '{}' so it indexes (set `distributions.{}` to pin a real distro)",
                            art_name, fmt, default_distro, fmt
                        ));
                        vec![default_distro.to_string()]
                    }
                    None => vec![String::new()],
                }
            } else {
                distro_slugs.clone()
            };

            // Per-file upload detail is verbose-only; the entry summary
            // reports the aggregate upload count at default verbosity.
            log.verbose(&format!(
                "uploading {} ({}, {} bytes, md5={}) → org '{}' repo '{}'{}",
                art_name,
                fmt,
                size_bytes,
                md5_hex,
                organization,
                repository,
                if distro_slugs.is_empty() {
                    String::new()
                } else {
                    format!(" distros={:?}", distro_slugs)
                },
            ));

            // --- Step 3/3 prep: package-create URL + component gating ---
            //
            // POST /v1/packages/{org}/{repo}/upload/{format}/ with the
            // identifier + distribution tells Cloudsmith to take the
            // uploaded raw file and register it as a deb/rpm/alpine
            // package. Without this step the bytes are dangling.
            //
            // When multiple distributions are configured
            // array form), step 3 is issued once per slug — CloudSmith's
            // API accepts only one `distribution` per call. Each
            // files/create slot (`identifier`) is consumed by a single
            // package-create, so the file stage (steps 1+2) runs once PER
            // distribution inside the loop — reusing one identifier across
            // distributions 4xx's on the 2nd+ call (the slot is spent).
            let package_upload_url = format!(
                "{}/packages/{}/{}/upload/{}/",
                api_base, organization, repository, fmt
            );
            let component_for_format = component
                .as_ref()
                .filter(|_| COMPONENT_BEARING_FORMATS.contains(&fmt));
            if component.is_some() && component_for_format.is_none() {
                log.verbose(&format!(
                    "cloudsmith component is set but format '{}' does not accept a component; dropping",
                    fmt
                ));
            }

            for distro in &upload_slugs {
                // Stage a fresh files/create slot + presigned upload for THIS
                // distribution. The identifier is single-use, so every
                // distribution needs its own.
                let identifier = stage_cloudsmith_file(
                    &client,
                    &api_base,
                    &organization,
                    &repository,
                    art_name,
                    &md5_hex,
                    &file_bytes,
                    &token,
                    &policy,
                    log,
                )?;

                let mut package_body = serde_json::json!({
                    "package_file": identifier,
                });
                if !distro.is_empty() {
                    package_body["distribution"] = serde_json::Value::String(distro.clone());
                }
                if let Some(comp) = component_for_format {
                    package_body["component"] = serde_json::Value::String(comp.clone());
                }
                if republish {
                    package_body["republish"] = serde_json::Value::Bool(true);
                }

                log.verbose(&format!(
                    "POST {} (identifier={}, distro={:?}, step 3 of 3)",
                    package_upload_url, identifier, distro
                ));
                let label = format!("packages/upload/{}", fmt);
                let step3_result = retry_request(&label, art_name, &policy, log, || {
                    client
                        .post(&package_upload_url)
                        .header("Authorization", format!("token {}", token))
                        .header("Accept", "application/json")
                        .json(&package_body)
                        .send()
                });

                let (pkg_status, pkg_body) = match step3_result {
                    Ok(pair) => pair,
                    Err(err) => {
                        // Race-recovery: a concurrent CI loop can submit the
                        // same name+version between our pre-check (or
                        // first-attempt step-3) and this step-3, returning
                        // 409/422 here. Without recovery, the upload aborts
                        // even though the operator's intent — "land this
                        // artifact on the registry" — was satisfied by the
                        // racing process. Re-query the remote: if it now
                        // exists with our md5, treat as idempotent skip; if
                        // it exists with a different md5, surface the same
                        // conflict the pre-check would have. Anything else
                        // (transport failure, 5xx after retries) propagates.
                        let status_in_chain: Option<u16> = err.chain().find_map(|e| {
                            e.downcast_ref::<anodizer_core::retry::HttpError>()
                                .map(|h| h.status)
                        });
                        let is_conflict = matches!(status_in_chain, Some(409) | Some(422));
                        if !is_conflict {
                            return Err(err);
                        }
                        log.warn(&format!(
                            "cloudsmith step-3 returned {:?} for '{}'; re-checking remote to \
                             decide between idempotent skip and real conflict",
                            status_in_chain, art_name
                        ));
                        match check_cloudsmith_package_exists(
                            &client,
                            &check_url,
                            &check_query,
                            &token,
                            art_name,
                            &md5_hex,
                            &policy,
                            log,
                        )? {
                            CloudsmithPackageState::SkipIdempotent => {
                                let msg = format!(
                                    "'{}' already landed on cloudsmith with matching md5 \
                                     (concurrent uploader); treating as idempotent skip",
                                    art_name
                                );
                                if republish {
                                    // A racing uploader landing the same bytes
                                    // while republish was requested is a real
                                    // surprise worth surfacing at default
                                    // verbosity.
                                    log.warn(&msg);
                                } else {
                                    log.verbose(&msg);
                                }
                                skipped_count += 1;
                                continue;
                            }
                            CloudsmithPackageState::Md5Mismatch { remote } => {
                                bail!(
                                    "cloudsmith: step-3 conflict for '{}' in org '{}' repo \
                                     '{}'; remote md5={} differs from local={}. A concurrent \
                                     upload submitted different bytes under the same name. \
                                     Set republish: true to force overwrite, or bump the \
                                     release.",
                                    art_name,
                                    organization,
                                    repository,
                                    remote,
                                    md5_hex
                                );
                            }
                            CloudsmithPackageState::NotFound => {
                                return Err(err);
                            }
                        }
                    }
                };

                let pkg_json = serde_json::from_str::<serde_json::Value>(&pkg_body).ok();
                let slug = pkg_json.as_ref().and_then(|v| {
                    v.get("slug_perm")
                        .or_else(|| v.get("slug"))
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string())
                });
                // Capture the CloudSmith package `name` so post-upload
                // `keep_versions` pruning can scope its list+delete to this
                // package alone (not siblings sharing the repo).
                if let Some(name) = pkg_json
                    .as_ref()
                    .and_then(|v| v.get("name"))
                    .and_then(|n| n.as_str())
                    && !name.is_empty()
                {
                    prune_package_names.insert(name.to_string());
                }
                // Per-file upload-success detail is verbose-only; the entry
                // summary reports the aggregate upload count.
                if let Some(ref s) = slug {
                    log.verbose(&format!(
                        "uploaded {} (slug={}{})",
                        art_name,
                        s,
                        if distro.is_empty() {
                            String::new()
                        } else {
                            format!(", distro={}", distro)
                        }
                    ));
                } else {
                    log.verbose(&format!("uploaded {} (HTTP {})", art_name, pkg_status));
                }
                uploaded_count += 1;
                uploaded.push(CloudsmithTarget {
                    org: organization.clone(),
                    repo: repository.clone(),
                    filename: art_name.to_string(),
                    slug,
                });
            }
        }

        log.status(&cloudsmith_upload_summary(
            uploaded_count,
            skipped_count,
            &organization,
            &repository,
        ));

        // --- Post-upload retention pruning (keep_versions) ---
        //
        // The upload (the real work) has already succeeded. Pruning is a
        // best-effort follow-up: a list/delete failure warns and continues —
        // it must NOT fail the stage or roll back the upload. `keep == 0` is
        // refused; unset (None) prunes nothing.
        if let Some(keep) = entry.keep_versions {
            if ctx.is_snapshot() {
                // Snapshot publishes are blocked by the shared non-release
                // version guard, but guard the destructive prune independently
                // so it can never delete real releases on behalf of a snapshot run.
                log.verbose("skipped cloudsmith keep_versions prune — snapshot mode");
            } else if keep == 0 {
                log.warn(
                    "skipped cloudsmith keep_versions prune — 0 is invalid (would prune every version)",
                );
            } else if ctx.version().is_empty() {
                // Without a known current version, the pure selector loses its
                // "always keep the just-uploaded version" safety net (an empty
                // version normalizes to "" and matches no bucket), so a ranking
                // quirk could delete what this run just uploaded. Refuse rather
                // than prune blind.
                log.warn(
                    "skipped cloudsmith keep_versions prune — current version is unknown (avoids deleting the just-uploaded release)",
                );
            } else {
                let current_version = ctx.version();
                for pkg_name in &prune_package_names {
                    prune_cloudsmith_versions(
                        &client,
                        &cloudsmith_api_base(),
                        &organization,
                        &repository,
                        pkg_name,
                        &current_version,
                        keep,
                        &token,
                        &policy,
                        log,
                    );
                }
            }
        }
    }

    Ok(uploaded)
}

/// List every version of a single CloudSmith package and DELETE those that
/// rank beyond the `keep` most-recent releases (`cloudsmiths[].keep_versions`).
///
/// Best-effort and non-fatal by contract: the upload already succeeded, so a
/// list or delete failure here emits a PROMINENT warning (visible at default
/// verbosity) naming what couldn't be pruned and returns without error — it
/// never fails the publish stage or triggers a rollback. The selection itself
/// is delegated to the pure [`select_versions_to_prune`] so the destructive
/// decision is unit-tested HTTP-free.
#[allow(clippy::too_many_arguments)]
fn prune_cloudsmith_versions(
    client: &reqwest::blocking::Client,
    api_base: &str,
    organization: &str,
    repository: &str,
    package_name: &str,
    current_version: &str,
    keep: u32,
    token: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
) {
    let list_url = format!("{}/packages/{}/{}/", api_base, organization, repository);
    // Filter server-side to THIS package name so sibling packages sharing the
    // repository are never listed (and therefore never pruned).
    let query = format!("name:{}", package_name);

    let entries = match list_cloudsmith_package_versions(
        client,
        &list_url,
        &query,
        package_name,
        token,
        policy,
        log,
    ) {
        Ok(e) => e,
        Err(err) => {
            log.warn(&format!(
                "cloudsmith keep_versions: could not list versions of '{}' in {}/{} ({}); \
                 NOTHING was pruned — older versions may still consume storage",
                package_name, organization, repository, err
            ));
            return;
        }
    };

    let slugs_to_delete = select_versions_to_prune(&entries, keep, current_version);
    if slugs_to_delete.is_empty() {
        log.verbose(&format!(
            "cloudsmith keep_versions: nothing to prune for '{}' (≤ {} versions present)",
            package_name, keep
        ));
        return;
    }

    let mut deleted = 0usize;
    let mut failed = 0usize;
    let mut failed_slugs: Vec<String> = Vec::new();
    for slug in &slugs_to_delete {
        let url = format!(
            "{}/packages/{}/{}/{}/",
            api_base, organization, repository, slug
        );
        log.verbose(&format!("DELETE {} (keep_versions prune)", url));
        match retry_request("packages/prune-delete", package_name, policy, log, || {
            client
                .delete(&url)
                .header("Authorization", format!("token {}", token))
                .header("Accept", "application/json")
                .send()
        }) {
            Ok(_) => deleted += 1,
            Err(err) => {
                // 404/410 = already gone (concurrent prune / manual delete):
                // count it as effectively pruned rather than a failure.
                let msg = format!("{err:#}");
                if msg.contains("HTTP 404") || msg.contains("HTTP 410") {
                    deleted += 1;
                } else {
                    failed += 1;
                    failed_slugs.push(slug.clone());
                    log.warn(&format!(
                        "cloudsmith keep_versions: failed to delete '{}' (slug {}): {}",
                        package_name, slug, err
                    ));
                }
            }
        }
    }

    // Summary of the distinct versions kept, for the operator-visible line.
    let kept_versions = retained_version_summary(&entries, keep, current_version);
    if failed == 0 {
        log.status(&format!(
            "pruned {} old artifact(s) of '{}' from cloudsmith (kept {} most-recent: {})",
            deleted, package_name, keep, kept_versions
        ));
    } else {
        log.warn(&format!(
            "cloudsmith keep_versions: pruned {} artifact(s) of '{}' but {} delete(s) FAILED \
             (slugs: {}); those older versions remain and still consume storage",
            deleted,
            package_name,
            failed,
            failed_slugs.join(", ")
        ));
    }
}

/// Human-readable list of the distinct normalized versions that survive a
/// `keep_versions` prune (the top `keep` plus the current upload), newest
/// first, for the operator summary line.
fn retained_version_summary(
    entries: &[CloudsmithVersionEntry],
    keep: u32,
    current_version: &str,
) -> String {
    let current_norm = normalize_cloudsmith_version(current_version);
    // Same comparator as the deletion decision so the "kept …" line can never
    // name a different version than the one actually retained.
    let (order, _buckets) = rank_distinct_versions_desc(entries);
    let mut kept: Vec<String> = order.iter().take(keep as usize).cloned().collect();
    if !kept.contains(&current_norm) {
        kept.push(current_norm);
    }
    kept.join(", ")
}

/// Page through the CloudSmith packages-list endpoint (filtered to one
/// package name) and project each entry into a [`CloudsmithVersionEntry`].
///
/// CloudSmith paginates at 100 results/page; a single package's
/// versions × formats × arches can exceed one page in a long-lived repo, so
/// this walks pages until a short (< page_size) page is returned. 4xx
/// fast-fails; 5xx/429/transport retry via the shared helper.
fn list_cloudsmith_package_versions(
    client: &reqwest::blocking::Client,
    list_url: &str,
    query: &str,
    package_name: &str,
    token: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<Vec<CloudsmithVersionEntry>> {
    const PAGE_SIZE: usize = 100;
    let mut out: Vec<CloudsmithVersionEntry> = Vec::new();
    let mut page = 1u32;
    loop {
        let page_str = page.to_string();
        let page_size_str = PAGE_SIZE.to_string();
        let (_status, body) =
            retry_request("packages/list (prune)", package_name, policy, log, || {
                client
                    .get(list_url)
                    .query(&[
                        ("query", query),
                        ("page", page_str.as_str()),
                        ("page_size", page_size_str.as_str()),
                    ])
                    .header("Authorization", format!("token {}", token))
                    .header("Accept", "application/json")
                    .send()
            })?;
        let parsed: serde_json::Value = serde_json::from_str(&body)
            .with_context(|| format!("cloudsmith: parse packages-list page {}", page))?;
        let array = match parsed.as_array() {
            Some(a) => a,
            None => break,
        };
        let page_len = array.len();
        for v in array {
            // Defensively re-filter by exact package name: the `query` is a
            // search term, not an exact match, so a substring sibling could
            // slip in. Only entries whose `name` equals our package are
            // candidates for pruning.
            let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if name != package_name {
                continue;
            }
            let slug = v
                .get("slug_perm")
                .or_else(|| v.get("slug"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if slug.is_empty() {
                continue;
            }
            let version = v.get("version").and_then(|s| s.as_str()).unwrap_or("");
            let uploaded_at = v
                .get("uploaded_at")
                .or_else(|| v.get("created_at"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            out.push(CloudsmithVersionEntry {
                slug: slug.to_string(),
                version: version.to_string(),
                uploaded_at: uploaded_at.to_string(),
            });
        }
        if page_len < PAGE_SIZE {
            break;
        }
        page += 1;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// CloudsmithPublisher (Publisher trait wrapper)
// ---------------------------------------------------------------------------

// Wraps [`publish_to_cloudsmith`] in the [`anodizer_core::Publisher`] trait
// so the new dispatch path (see [`crate::registry::configured_publishers`])
// can drive Cloudsmith uploads alongside every other publisher.
//
// Group: [`anodizer_core::PublisherGroup::Assets`] (uploadable packages,
// server-side deletable). `required = false`.
//
// Rollback shape: per uploaded package, issue
// `DELETE /v1/packages/<org>/<repo>/<slug>/` with the same `CLOUDSMITH_*`
// token used for the upload. The slug is the per-package permanent
// identifier returned by the step-3 `packages/upload/<format>/` response
// and is captured into `CloudsmithTarget.slug` so [`PublishEvidence::extra`]
// (`cloudsmith_targets` key) carries it across the publish/rollback split.
// Targets whose slug couldn't be parsed (older evidence written before
// B13, or a response-shape change) degrade to the warn-only manual-cleanup
// checklist via [`cloudsmith_manual_cleanup_msg`]. Per-target DELETE
// failures (non-2xx, transport errors) emit a warn and continue —
// rollback is best-effort and a single 5xx must not orphan the remaining
// packages.
simple_publisher!(
    CloudsmithPublisher,
    "cloudsmith",
    anodizer_core::PublisherGroup::Assets,
    false,
    Some("CLOUDSMITH_API_KEY package_delete"),
);

/// One Cloudsmith upload target as recorded in evidence. Operator-readable
/// `(org, repo, filename)` tuples drive the rollback warn line; the optional
/// `slug` (Cloudsmith's per-package permanent identifier, returned by the
/// step-3 `packages/upload/<format>/` response) lets [`rollback`] issue a
/// real `DELETE /v1/packages/<org>/<repo>/<slug>/` instead of a warn-only
/// manual-cleanup checklist.
///
/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// have no slot to land in. `slug` stays `Option` because evidence
/// emitted before slug-capture didn't carry it; rollback falls back
/// to the warn-only path (see [`cloudsmith_manual_cleanup_msg`]) for
/// any target whose slug is absent.
pub(crate) type CloudsmithTarget = anodizer_core::publish_evidence::CloudsmithTargetSnapshot;

/// Encode the per-target tuples into the typed
/// [`PublishEvidenceExtra::Cloudsmith`] variant.
pub(crate) fn encode_cloudsmith_targets(
    targets: &[CloudsmithTarget],
) -> anodizer_core::PublishEvidenceExtra {
    anodizer_core::PublishEvidenceExtra::Cloudsmith(
        anodizer_core::publish_evidence::CloudsmithExtra {
            cloudsmith_targets: targets.to_vec(),
        },
    )
}

/// Decode the typed Cloudsmith variant back into structured targets.
/// Returns an empty vec when the variant doesn't match — the rollback
/// then surfaces the empty-evidence warn instead of crashing.
pub(crate) fn decode_cloudsmith_targets(
    extra: &anodizer_core::PublishEvidenceExtra,
) -> Vec<CloudsmithTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Cloudsmith(c) => c.cloudsmith_targets.clone(),
        _ => Vec::new(),
    }
}

/// The per-target warn line a rollback emits as a FALLBACK when no slug is
/// available in evidence (legacy evidence written before B13 added slug
/// capture, or a step-3 `packages/upload/<format>/` response that didn't
/// surface a slug). Operator-readable; renders the load-bearing
/// `<org>/<repo>` location plus the filename to remove. Exposed as a
/// helper so tests can pin the wording without intercepting stderr.
///
/// The PRIMARY rollback path issues a real
/// `DELETE /v1/packages/<org>/<repo>/<slug>/` against the Cloudsmith API
/// (see [`<CloudsmithPublisher as anodizer_core::Publisher>::rollback`]);
/// this helper is reached only when `target.slug` is `None`.
pub(crate) fn cloudsmith_manual_cleanup_msg(target: &CloudsmithTarget) -> String {
    format!(
        "manually withdraw '{}' from cloudsmith {}/{} (per-package slug not surfaced in evidence; delete via the Cloudsmith dashboard)",
        target.filename, target.org, target.repo
    )
}

impl anodizer_core::Publisher for CloudsmithPublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }

    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }

    fn required(&self) -> bool {
        Self::resolved_required(self)
    }

    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }

    fn requirements(&self, ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
        // Same env-var-name resolution the upload path uses: a (templated)
        // `secret_name` per entry, defaulting to CLOUDSMITH_TOKEN.
        ctx.config
            .cloudsmiths
            .iter()
            .flatten()
            .filter(|entry| {
                !crate::publisher_helpers::entry_inactive(
                    ctx,
                    entry.skip.as_ref(),
                    None,
                    entry.if_condition.as_deref(),
                )
            })
            .map(|entry| {
                let var = crate::util::resolve_secret_name(
                    ctx,
                    entry.secret_name.as_deref(),
                    "CLOUDSMITH_TOKEN",
                );
                anodizer_core::EnvRequirement::EnvAllOf { vars: vec![var] }
            })
            .collect()
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        // The upload path returns the live target list (with slugs
        // populated when step 3's response carried one) so evidence
        // records what we actually uploaded — not a post-hoc walk of
        // config + artifacts, which can drift from the upload list and
        // never captures the slug. SkipIdempotent matches (artifact
        // already on Cloudsmith with matching md5) are NOT in `targets`
        // because rollback only undoes what THIS run did.
        let targets = publish_to_cloudsmith(ctx, &log)?;
        let mut evidence = anodizer_core::PublishEvidence::new("cloudsmith");
        // The `artifact_paths` slot keeps the operator-readable
        // `<org>/<repo>/<filename>` form for the text-only
        // --rollback-only summary; the structured copy in `extra` is the
        // authoritative source for the DELETE call.
        let path_view: Vec<std::path::PathBuf> = targets
            .iter()
            .map(|t| std::path::PathBuf::from(format!("{}/{}/{}", t.org, t.repo, t.filename)))
            .collect();
        if let Some(first) = path_view.first() {
            evidence.primary_ref = Some(first.display().to_string());
        }
        evidence.artifact_paths = path_view;
        evidence.extra = encode_cloudsmith_targets(&targets);
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_cloudsmith_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "cloudsmith",
                "upload targets",
            ));
            return Ok(());
        }

        // Resolve the API token once; if it's absent we cannot DELETE
        // anything, so fall back to the warn-only manual-cleanup
        // checklist for every target. `CLOUDSMITH_API_KEY` is the
        // rollback-scope env name declared by `rollback_scope_needed`.
        let token = ctx.env_var("CLOUDSMITH_API_KEY");
        if token.is_none() {
            log.warn(
                "CLOUDSMITH_API_KEY not set; emitting manual-cleanup checklist instead of DELETE",
            );
        }

        let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(30))
            .context("cloudsmith: failed to build HTTP client for rollback")?;
        let policy = ctx.retry_policy();

        let mut deleted = 0usize;
        let mut already_absent = 0usize;
        let mut failed = 0usize;
        let mut warn_only = 0usize;

        for target in &targets {
            // Two ways into the warn-only fallback:
            //   1. No token at all (handled above; warn already emitted).
            //   2. No slug for this target (older evidence, or step-3
            //      response shape change).
            let Some(slug) = target.slug.as_deref() else {
                log.warn(&cloudsmith_manual_cleanup_msg(target));
                warn_only += 1;
                continue;
            };
            let Some(tok) = token.as_deref() else {
                log.warn(&cloudsmith_manual_cleanup_msg(target));
                warn_only += 1;
                continue;
            };

            let url = format!(
                "{}/packages/{}/{}/{}/",
                cloudsmith_api_base(),
                target.org,
                target.repo,
                slug
            );
            log.status(&format!("DELETE {}", url));
            let label = "packages/delete";
            match retry_request(label, &target.filename, &policy, &log, || {
                client
                    .delete(&url)
                    .header("Authorization", format!("token {}", tok))
                    .header("Accept", "application/json")
                    .send()
            }) {
                Ok((status, _body)) => {
                    if status.is_success() {
                        deleted += 1;
                    } else {
                        // `retry_http_blocking` Strict mode treats only
                        // 2xx as success, so 4xx (other than 404/410) and
                        // 5xx already raise an `Err` here. This arm is
                        // unreachable, but guard it defensively.
                        failed += 1;
                        log.warn(&format!(
                            "DELETE {} returned HTTP {} (manual cleanup may be required)",
                            url, status
                        ));
                    }
                }
                Err(err) => {
                    // 404 / 410 = package already absent (operator deleted
                    // via the dashboard, or a prior partial rollback ran).
                    // Detect by substring on the shaped error message.
                    let msg = format!("{err:#}");
                    if msg.contains("HTTP 404") || msg.contains("HTTP 410") {
                        already_absent += 1;
                        log.status(&format!("DELETE {} already absent (404/410)", url));
                    } else {
                        failed += 1;
                        log.warn(&format!(
                            "DELETE {} failed ({}); manual cleanup may be required",
                            url, err
                        ));
                    }
                }
            }
        }

        log.status(&format!(
            "cloudsmith rollback complete — {} deleted, {} already absent, {} failed, {} warn-only (slug/token unavailable)",
            deleted, already_absent, failed, warn_only
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }

    fn skips_on_nightly(&self) -> bool {
        // Cloudsmith supports versioned packages; nightly uploads do not
        // clobber stable content and are allowed.
        false
    }

    fn retain_on_rollback(&self) -> bool {
        Self::resolved_retain_on_rollback(self)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodizer_core::artifact::Artifact;
    use anodizer_core::config::{CloudSmithConfig, Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    use std::path::PathBuf;

    fn dry_run_ctx(config: Config) -> Context {
        Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        )
    }

    /// The per-entry summary reports the uploaded artifact count and the
    /// already-present skip count taken from the loop's own tallies, so an
    /// entry that uploaded 4 and skipped 1 renders `uploaded 4 …, skipped 1`.
    /// CloudSmith requires a distribution for deb/alpine; when the user
    /// configures none, the accept-all catch-all keeps the package
    /// installable. rpm/srpm/raw need no distribution and return None.
    #[test]
    fn cloudsmith_default_distribution_per_format() {
        assert_eq!(
            cloudsmith_default_distribution("deb"),
            Some("any-distro/any-version")
        );
        assert_eq!(
            cloudsmith_default_distribution("alpine"),
            Some("alpine/any-version")
        );
        assert_eq!(cloudsmith_default_distribution("rpm"), None);
        assert_eq!(cloudsmith_default_distribution("srpm"), None);
        assert_eq!(cloudsmith_default_distribution("raw"), None);
    }

    #[test]
    fn upload_summary_reflects_uploaded_and_skipped_counts() {
        let line = cloudsmith_upload_summary(4, 1, "acme", "stable");
        assert_eq!(
            line,
            "uploaded 4 artifact(s), skipped 1 (already present) → acme/stable"
        );
    }

    /// A fully-idempotent re-run (nothing newly uploaded) still renders a
    /// factual summary with a zero upload count rather than suppressing it.
    #[test]
    fn upload_summary_handles_zero_uploads() {
        let line = cloudsmith_upload_summary(0, 3, "acme", "edge");
        assert_eq!(
            line,
            "uploaded 0 artifact(s), skipped 3 (already present) → acme/edge"
        );
    }

    fn entry(slug: &str, version: &str, uploaded_at: &str) -> CloudsmithVersionEntry {
        CloudsmithVersionEntry {
            slug: slug.to_string(),
            version: version.to_string(),
            uploaded_at: uploaded_at.to_string(),
        }
    }

    // keep_versions=2 over 4 versions: the 2 oldest are deleted, the 2
    // newest (incl. the current upload) are kept.
    #[test]
    fn prune_keep_2_of_4_deletes_two_oldest() {
        let entries = vec![
            entry("s-070", "0.7.0", "2026-06-13T00:00:00Z"),
            entry("s-061", "0.6.1", "2026-05-13T00:00:00Z"),
            entry("s-060", "0.6.0", "2026-04-13T00:00:00Z"),
            entry("s-050", "0.5.0", "2026-03-13T00:00:00Z"),
        ];
        let mut to_delete = select_versions_to_prune(&entries, 2, "0.7.0");
        to_delete.sort();
        assert_eq!(to_delete, vec!["s-050".to_string(), "s-060".to_string()]);
    }

    // keep_versions never deletes the current upload even when ranking would
    // otherwise rank it out of the top-N (e.g. a hotfix re-cut of an older
    // line, or skewed timestamps).
    #[test]
    fn prune_never_deletes_current_version() {
        let entries = vec![
            entry("s-090", "0.9.0", "2026-06-13T00:00:00Z"),
            entry("s-081", "0.8.1", "2026-06-12T00:00:00Z"),
            entry("s-080", "0.8.0", "2026-06-11T00:00:00Z"),
        ];
        // keep=1 would normally keep only 0.9.0, but current is 0.8.0.
        let to_delete = select_versions_to_prune(&entries, 1, "0.8.0");
        assert!(
            !to_delete.contains(&"s-080".to_string()),
            "current version must never be pruned: {to_delete:?}"
        );
        // 0.9.0 (top-1) and 0.8.0 (current) kept; 0.8.1 pruned.
        assert_eq!(to_delete, vec!["s-081".to_string()]);
    }

    // All formats of one release (deb epoch `1:0.9.1-1`, apk `0.9.1-r1`, rpm
    // `0.9.1-1`, bare `0.9.1`) normalize to `0.9.1` and rank as ONE version.
    #[test]
    fn prune_normalizes_epoch_and_revision_into_one_version() {
        let entries = vec![
            entry("deb-091", "1:0.9.1-1", "2026-06-13T00:00:00Z"),
            entry("apk-091", "0.9.1-r1", "2026-06-13T00:00:00Z"),
            entry("rpm-091", "0.9.1-1", "2026-06-13T00:00:00Z"),
            entry("deb-090", "1:0.9.0-1", "2026-05-13T00:00:00Z"),
            entry("apk-090", "0.9.0-r1", "2026-05-13T00:00:00Z"),
        ];
        // keep=1, current 0.9.1 → keep all three 0.9.1 artifacts, prune both
        // 0.9.0 artifacts.
        let mut to_delete = select_versions_to_prune(&entries, 1, "0.9.1");
        to_delete.sort();
        assert_eq!(
            to_delete,
            vec!["apk-090".to_string(), "deb-090".to_string()]
        );
    }

    #[test]
    fn normalize_strips_epoch_and_revision() {
        assert_eq!(normalize_cloudsmith_version("1:0.9.1-1"), "0.9.1");
        assert_eq!(normalize_cloudsmith_version("0.9.1-r1"), "0.9.1");
        assert_eq!(normalize_cloudsmith_version("0.9.1-1"), "0.9.1");
        assert_eq!(normalize_cloudsmith_version("0.9.1"), "0.9.1");
        assert_eq!(normalize_cloudsmith_version("2:1.2.3-5"), "1.2.3");
        // SemVer prerelease tails survive (not a packaging revision).
        assert_eq!(normalize_cloudsmith_version("0.9.1-rc.1"), "0.9.1-rc.1");
        assert_eq!(normalize_cloudsmith_version("0.9.1-alpha"), "0.9.1-alpha");
        // A non-numeric prerelease tail is NOT a packaging revision and
        // survives intact (head `0.9.1` is bare SemVer but tail `beta` isn't
        // `r?<digits>`).
        assert_eq!(normalize_cloudsmith_version("0.9.1-beta"), "0.9.1-beta");
        // A deb revision ON a prerelease strips only the trailing revision,
        // keeping the prerelease: head `1.0.0-rc.1` parses as SemVer, tail `1`
        // is a numeric revision. `1.0.0-rc-1` likewise → `1.0.0-rc`.
        assert_eq!(normalize_cloudsmith_version("1.0.0-rc.1-1"), "1.0.0-rc.1");
        assert_eq!(normalize_cloudsmith_version("1.0.0-rc-1"), "1.0.0-rc");
        // A tail that isn't `r?<digits>` is never stripped even with a SemVer
        // head, so a true single-segment prerelease is safe.
        assert_eq!(normalize_cloudsmith_version("1.0.0-rc"), "1.0.0-rc");
    }

    // The operator "kept …" summary must name exactly the versions that
    // survive deletion — both go through the same comparator.
    #[test]
    fn retained_summary_matches_selection() {
        let entries = vec![
            entry("s-100", "1.0.0", "2026-06-13T00:00:00Z"),
            entry("s-091", "0.9.1", "2026-05-13T00:00:00Z"),
            entry("s-090", "0.9.0", "2026-04-13T00:00:00Z"),
        ];
        let to_delete = select_versions_to_prune(&entries, 2, "1.0.0");
        // 1.0.0 + 0.9.1 kept, 0.9.0 pruned.
        assert_eq!(to_delete, vec!["s-090".to_string()]);
        let summary = retained_version_summary(&entries, 2, "1.0.0");
        assert_eq!(summary, "1.0.0, 0.9.1");
    }

    // keep=0 refuses to prune anything (belt-and-braces; caller rejects 0).
    #[test]
    fn prune_keep_zero_deletes_nothing() {
        let entries = vec![
            entry("s-090", "0.9.0", "2026-06-13T00:00:00Z"),
            entry("s-080", "0.8.0", "2026-06-12T00:00:00Z"),
        ];
        assert!(select_versions_to_prune(&entries, 0, "0.9.0").is_empty());
    }

    // Versions that won't parse as SemVer fall back to uploaded_at ordering
    // and rank below any parseable version.
    #[test]
    fn prune_unparseable_versions_fall_back_to_timestamp() {
        let entries = vec![
            entry("s-good", "1.0.0", "2026-01-01T00:00:00Z"),
            entry("s-new", "nightly-xyz", "2026-06-13T00:00:00Z"),
            entry("s-old", "nightly-abc", "2026-05-13T00:00:00Z"),
        ];
        // keep=2, current 1.0.0: parseable 1.0.0 ranks first and is kept;
        // among the two unparseable, the newer (s-new) takes the 2nd slot,
        // so s-old is pruned.
        let to_delete = select_versions_to_prune(&entries, 2, "1.0.0");
        assert_eq!(to_delete, vec!["s-old".to_string()]);
    }

    #[test]
    fn test_cloudsmith_skips_when_no_config() {
        let config = Config::default();
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_skips_when_empty_vec() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_skips_when_skipped() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_skips_when_skip_string_true() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            skip: Some(StringOrBool::String("true".to_string())),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_requires_organization() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: None,
            repository: Some("myrepo".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        let err = publish_to_cloudsmith(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("'organization' is required"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_cloudsmith_requires_organization_nonempty() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some(String::new()),
            repository: Some("myrepo".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        let err = publish_to_cloudsmith(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("'organization' is required"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_cloudsmith_requires_repository() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: None,
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        let err = publish_to_cloudsmith(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("'repository' is required"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_cloudsmith_requires_repository_nonempty() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some(String::new()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        let err = publish_to_cloudsmith(&ctx, &log).unwrap_err();
        assert!(
            err.to_string().contains("'repository' is required"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_cloudsmith_upload_url() {
        // Display-only helper (dry-run logs). Live code uses the 3-step API
        // flow against api.cloudsmith.io, not a single upload URL.
        let url = cloudsmith_upload_url("myorg", "myrepo", "deb", "ubuntu/focal");
        assert_eq!(
            url,
            format!(
                "{}/packages/myorg/myrepo/upload/deb/ (distribution=ubuntu/focal)",
                CLOUDSMITH_API_BASE
            )
        );
    }

    #[test]
    fn test_cloudsmith_default_formats() {
        let defaults = cloudsmith_default_formats();
        assert_eq!(defaults, vec!["apk", "deb", "rpm"]);
    }

    #[test]
    fn test_cloudsmith_dry_run() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            formats: Some(vec!["deb".to_string()]),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_dry_run_default_formats() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            formats: None,
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_dry_run_with_ids_filter() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            ids: Some(vec!["build1".to_string(), "build2".to_string()]),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_dry_run_with_distributions() {
        use anodizer_core::config::CloudSmithDistributions;

        let mut distributions = HashMap::new();
        distributions.insert(
            "deb".to_string(),
            CloudSmithDistributions::Single("ubuntu/focal".to_string()),
        );

        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            distributions: Some(distributions),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    /// YAML array form (`deb: ["ubuntu/focal", "ubuntu/jammy"]`) parses
    /// into [`CloudSmithDistributions::Multiple`].
    #[test]
    fn distributions_array_form_parses() {
        use anodizer_core::config::CloudSmithDistributions;
        let yaml = "deb:\n  - ubuntu/focal\n  - ubuntu/jammy\n";
        let parsed: HashMap<String, CloudSmithDistributions> =
            serde_yaml_ng::from_str(yaml).unwrap();
        match parsed.get("deb").unwrap() {
            CloudSmithDistributions::Multiple(v) => {
                assert_eq!(
                    v,
                    &vec!["ubuntu/focal".to_string(), "ubuntu/jammy".to_string()]
                );
            }
            other => panic!("expected Multiple, got {:?}", other),
        }
    }

    /// `.src.rpm` files map to the `srpm` format slug (NOT `rpm`).
    #[test]
    fn detect_format_distinguishes_src_rpm() {
        assert_eq!(detect_format("pkg-1.0-1.src.rpm"), "srpm");
        assert_eq!(detect_format("pkg-1.0-1.x86_64.rpm"), "rpm");
        assert_eq!(
            detect_format("pkg-1.0-1.SRC.rpm"),
            "srpm",
            "case-insensitive"
        );
    }

    /// `cloudsmith_format_matches` accepts both `apk` (user-facing) and
    /// `alpine` (API-side) spellings.
    #[test]
    fn format_matches_apk_and_alpine_aliases() {
        assert!(cloudsmith_format_matches("pkg.apk", &["apk".to_string()]));
        assert!(cloudsmith_format_matches(
            "pkg.apk",
            &["alpine".to_string()]
        ));
    }

    /// `cloudsmith_format_matches` recognises both `srpm` and `src.rpm`
    /// filter slugs against a `.src.rpm` file.
    #[test]
    fn format_matches_srpm_aliases() {
        assert!(cloudsmith_format_matches(
            "pkg-1.0-1.src.rpm",
            &["srpm".to_string()]
        ));
        assert!(cloudsmith_format_matches(
            "pkg-1.0-1.src.rpm",
            &["src.rpm".to_string()]
        ));
    }

    #[test]
    fn test_cloudsmith_dry_run_with_component() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            component: Some("main".to_string()),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_dry_run_with_republish() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            republish: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_dry_run_default_secret_name() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            secret_name: None,
            ..Default::default()
        }]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_multiple_entries() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![
            CloudSmithConfig {
                organization: Some("org1".to_string()),
                repository: Some("repo1".to_string()),
                ..Default::default()
            },
            CloudSmithConfig {
                organization: Some("org2".to_string()),
                repository: Some("repo2".to_string()),
                skip: Some(StringOrBool::Bool(true)),
                ..Default::default()
            },
        ]);
        let ctx = dry_run_ctx(config);
        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    #[test]
    fn test_cloudsmith_live_mode_errors_without_token() {
        let mut config = Config::default();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            secret_name: Some("CLOUDSMITH_TEST_NONEXISTENT_TOKEN_12345".to_string()),
            ..Default::default()
        }]);
        let ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        let log = ctx.logger("cloudsmith");
        let result = publish_to_cloudsmith(&ctx, &log);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("CLOUDSMITH_TEST_NONEXISTENT_TOKEN_12345"),
            "error should mention the secret env var name, got: {}",
            msg
        );
    }

    #[test]
    fn test_cloudsmith_format_matches() {
        let formats = vec!["deb".to_string(), "rpm".to_string()];
        assert!(cloudsmith_format_matches("myapp_1.0.0_amd64.deb", &formats));
        assert!(cloudsmith_format_matches(
            "myapp-1.0.0.x86_64.rpm",
            &formats
        ));
        assert!(!cloudsmith_format_matches("myapp-1.0.0.tar.gz", &formats));
    }

    #[test]
    fn test_cloudsmith_format_matches_apk() {
        let formats = vec!["apk".to_string()];
        assert!(cloudsmith_format_matches("myapp-1.0.0.apk", &formats));
        assert!(!cloudsmith_format_matches("myapp-1.0.0.deb", &formats));
    }

    #[test]
    fn test_cloudsmith_format_matches_empty_formats() {
        let formats: Vec<String> = vec![];
        assert!(!cloudsmith_format_matches("myapp.deb", &formats));
    }

    #[test]
    fn test_detect_format() {
        assert_eq!(detect_format("app.deb"), "deb");
        assert_eq!(detect_format("app.rpm"), "rpm");
        assert_eq!(detect_format("app.apk"), "alpine");
        assert_eq!(detect_format("app.tar.gz"), "raw");
    }

    #[test]
    fn test_cloudsmith_dry_run_lists_matching_artifacts() {
        let mut config = Config::default();
        config.project_name = "testapp".to_string();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            ..Default::default()
        }]);
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::LinuxPackage,
            name: "testapp_1.0.0_amd64.deb".to_string(),
            path: PathBuf::from("dist/testapp_1.0.0_amd64.deb"),
            target: None,
            crate_name: "testapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::LinuxPackage,
            name: "testapp-1.0.0.x86_64.rpm".to_string(),
            path: PathBuf::from("dist/testapp-1.0.0.x86_64.rpm"),
            target: None,
            crate_name: "testapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let log = ctx.logger("cloudsmith");
        assert!(publish_to_cloudsmith(&ctx, &log).is_ok());
    }

    /// Defense-in-depth: a Cloudsmith API error response that echoes our
    /// `Authorization: Bearer <PAT>` header back must not leak the token
    /// into the user-visible error chain. Exercises the `retry_request`
    /// helper's error-message closure via a one-shot TCP responder.
    #[test]
    fn retry_request_redacts_bearer_in_error_body() {
        use anodizer_core::log::{StageLogger, Verbosity};
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        use std::time::Duration;

        let leaky = "Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg";
        let body_len = leaky.len();
        let resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {body_len}\r\n\r\n{leaky}"
            )
            .into_boxed_str(),
        );

        // Serve up to 3 identical attempts (matches fast_policy max_attempts).
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp; 3]);

        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let log = StageLogger::new("cloudsmith", Verbosity::Normal);
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let url = format!("http://{addr}/files/");
        let err = retry_request("upload", "test.deb", &policy, &log, || {
            client.post(&url).send()
        })
        .expect_err("500 must exhaust + error");
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

    /// Multi-distribution upload must stage a fresh files/create slot +
    /// presigned upload PER distribution: a Cloudsmith identifier is consumed
    /// by a single package-create, so reusing one across distributions makes
    /// the 2nd+ package-create 4xx.
    ///
    /// Two distributions ⇒ each needs its own (files/create + presigned +
    /// package-create) = 6 served connections. The bug (file stage hoisted
    /// out of the loop) would serve only 4 (1 files/create + 1 presigned +
    /// 2 package-creates). The connection count is the load-bearing assertion.
    #[test]
    #[serial_test::serial]
    fn cloudsmith_multi_distribution_stages_one_file_per_distro() {
        use anodizer_core::MapEnvSource;
        use anodizer_core::config::CloudSmithDistributions;
        use anodizer_core::log::{StageLogger, Verbosity};
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder_with;
        use std::sync::atomic::Ordering;

        let tmp = tempfile::tempdir().unwrap();
        let art_path = tmp.path().join("app_1.0.0_amd64.deb");
        std::fs::write(&art_path, b"fake-deb-bytes").unwrap();

        let http_json = |body: String| -> String {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
        };
        let presigned_ok = || "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_string();

        // Build the response queue AFTER the responder binds so the
        // files/create `upload_url` can point the presigned upload (step 2)
        // back at this same responder. Served one-per-connection, in order;
        // per distribution the client opens three connections in sequence:
        //   files/create -> presigned upload -> packages/upload.
        let (addr, calls) = spawn_oneshot_http_responder_with(|addr| {
            let base = format!("http://{addr}");
            let files_create = |id: &str| {
                http_json(format!(
                    r#"{{"identifier":"{id}","upload_url":"{base}/s3-presigned/","upload_fields":{{"key":"v"}}}}"#
                ))
            };
            vec![
                files_create("id-distro-1"),
                presigned_ok(),
                http_json(r#"{"slug_perm":"slug-1"}"#.to_string()),
                files_create("id-distro-2"),
                presigned_ok(),
                http_json(r#"{"slug_perm":"slug-2"}"#.to_string()),
            ]
        });
        let base = format!("http://{addr}");

        let mut distros: HashMap<String, CloudSmithDistributions> = HashMap::new();
        distros.insert(
            "deb".to_string(),
            CloudSmithDistributions::Multiple(vec![
                "ubuntu/focal".to_string(),
                "ubuntu/jammy".to_string(),
            ]),
        );

        let mut config = Config::default();
        config.project_name = "app".to_string();
        config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            distributions: Some(distros),
            // republish=true skips the pre-check packages-list query so the
            // response queue stays exactly the 3-per-distro upload sequence.
            republish: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }]);

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.set_env_source(
            MapEnvSource::new()
                .with("CLOUDSMITH_TOKEN", "fake-token")
                .with("ANODIZE_CLOUDSMITH_API_BASE", &base),
        );
        // `cloudsmith_api_base()` reads the process env (not ctx.env_var),
        // so the base override must be set there too. Serialized via #[serial].
        unsafe {
            std::env::set_var("ANODIZE_CLOUDSMITH_API_BASE", &base);
        }

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::LinuxPackage,
            name: "app_1.0.0_amd64.deb".to_string(),
            path: art_path.clone(),
            target: None,
            crate_name: "app".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let log = StageLogger::new("cloudsmith", Verbosity::Quiet);
        let result = publish_to_cloudsmith(&ctx, &log);

        unsafe {
            std::env::remove_var("ANODIZE_CLOUDSMITH_API_BASE");
        }

        let uploaded = result.expect("multi-distribution upload should succeed");
        // One CloudsmithTarget recorded per distribution package-create.
        assert_eq!(
            uploaded.len(),
            2,
            "expected one recorded target per distribution, got {uploaded:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            6,
            "two distributions must each stage their own file (3 connections \
             each: files/create + presigned + package-create); a hoisted file \
             stage would serve only 4"
        );
    }

    // ---- classify_cloudsmith_package_response ----------------------------
    //
    // Pure-function tests for the packages-list response classifier. The
    // network-bound `check_cloudsmith_package_exists` is exercised
    // indirectly via the same retry helper as `retry_request` (already
    // covered above); these tests pin the JSON decision rule.

    #[test]
    fn cloudsmith_classify_not_found_when_empty_array() {
        let result =
            classify_cloudsmith_package_response("[]", "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(result, CloudsmithPackageState::NotFound);
    }

    #[test]
    fn cloudsmith_classify_not_found_when_no_matching_filename() {
        let body = r#"[{"filename":"other.deb","checksum_md5":"abcd"}]"#;
        let result =
            classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(result, CloudsmithPackageState::NotFound);
    }

    #[test]
    fn cloudsmith_classify_skip_when_md5_matches() {
        let body = r#"[{"filename":"app_1.0.0_amd64.deb","checksum_md5":"deadbeef"}]"#;
        let result =
            classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(result, CloudsmithPackageState::SkipIdempotent);
    }

    #[test]
    fn cloudsmith_classify_skip_when_md5_matches_case_insensitive() {
        // Cloudsmith may return uppercase hex; our local computation is
        // lowercase. The comparator must normalize.
        let body = r#"[{"filename":"app_1.0.0_amd64.deb","checksum_md5":"DEADBEEF"}]"#;
        let result =
            classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(result, CloudsmithPackageState::SkipIdempotent);
    }

    #[test]
    fn cloudsmith_classify_skip_when_md5_field_absent() {
        // Filename match but no checksum_md5 in the response — presence is
        // a strong-enough idempotency signal; uploading would create a
        // duplicate package with a different md5.
        let body = r#"[{"filename":"app_1.0.0_amd64.deb"}]"#;
        let result =
            classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(result, CloudsmithPackageState::SkipIdempotent);
    }

    #[test]
    fn cloudsmith_classify_bails_when_md5_differs() {
        // The scenario the pre-check guards: a previous run uploaded with
        // one md5, the retry's re-packaged artifact has a different md5.
        // Bail loudly instead of creating a conflicting duplicate.
        let body = r#"[{"filename":"app_1.0.0_amd64.deb","checksum_md5":"aaaa1111"}]"#;
        let result =
            classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(
            result,
            CloudsmithPackageState::Md5Mismatch {
                remote: "aaaa1111".to_string()
            }
        );
    }

    #[test]
    fn cloudsmith_classify_handles_non_array_body() {
        // An error envelope or unexpected shape: treat as NotFound rather
        // than blow up, since we can't fix the mismatch anyway and a false
        // upload-attempt is recoverable while a false bail is not.
        let body = r#"{"detail":"not authorized"}"#;
        let result =
            classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(result, CloudsmithPackageState::NotFound);
    }

    #[test]
    fn cloudsmith_classify_picks_first_matching_filename() {
        // Defensive: if Cloudsmith returns multiple entries (e.g. across
        // distributions), the classifier picks the first match. Both
        // entries have the same md5 here, mirroring real-world behavior
        // where the same filename is shared across distros.
        let body = r#"[
            {"filename":"other.deb","checksum_md5":"abcd"},
            {"filename":"app_1.0.0_amd64.deb","checksum_md5":"deadbeef"},
            {"filename":"app_1.0.0_amd64.deb","checksum_md5":"deadbeef"}
        ]"#;
        let result =
            classify_cloudsmith_package_response(body, "app_1.0.0_amd64.deb", "deadbeef").unwrap();
        assert_eq!(result, CloudsmithPackageState::SkipIdempotent);
    }

    // ---- live 3-step upload path (scripted_responder) --------------------
    //
    // These tests redirect ALL Cloudsmith API traffic to an in-process TCP
    // responder via the `ANODIZE_CLOUDSMITH_API_BASE` env seam, then drive a
    // real `publish_to_cloudsmith` and assert on the recorded request log
    // (method / path / body). Every one mutates the process env (the base
    // override) so all are `#[serial_test::serial]` and hold the shared
    // `env_mutex` across the publish call — `cloudsmith_api_base()` reads
    // `std::env::var`, not `ctx.env_var`, so the override must live in the
    // process env for the duration of the run.

    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::env::env_mutex;
    use anodizer_core::test_helpers::scripted_responder::{
        RequestLog, ScriptedRoute, spawn_scripted_responder_with,
    };
    use anodizer_core::{MapEnvSource, config::RetryConfig};

    /// A `retry:` block with millisecond delays so a 5xx-then-success test
    /// retries without the default 10s base sleep stretching CI.
    fn fast_retry_config() -> RetryConfig {
        use anodizer_core::config::HumanDuration;
        RetryConfig {
            attempts: 3,
            delay: HumanDuration(std::time::Duration::from_millis(1)),
            max_delay: HumanDuration(std::time::Duration::from_millis(5)),
        }
    }

    /// `HTTP/1.1 200` envelope wrapping a JSON body.
    fn http_json(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    }

    /// Static `204 No Content` for the S3 presigned upload (step 2).
    const PRESIGNED_204: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";

    /// md5 the bytes the same way production does, so a pre-check response
    /// can be made to match (or deliberately not match) the local digest.
    fn md5_hex_of(bytes: &[u8]) -> String {
        use md5::Digest as _;
        let mut h = md5::Md5::new();
        h.update(bytes);
        anodizer_core::hashing::hex_lower(&h.finalize())
    }

    /// Build a single-artifact Context whose token resolves from an injected
    /// env source (no process-env mutation needed for the token — only the
    /// API base override touches the process env, handled per-test).
    fn ctx_with_one_artifact(
        cfg: CloudSmithConfig,
        base: &str,
        kind: ArtifactKind,
        art_name: &str,
        path: PathBuf,
        retry: Option<RetryConfig>,
    ) -> Context {
        let mut config = Config::default();
        config.project_name = "app".to_string();
        config.retry = retry;
        config.cloudsmiths = Some(vec![cfg]);
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.set_env_source(
            MapEnvSource::new()
                .with("CLOUDSMITH_TOKEN", "fake-token")
                .with("ANODIZE_CLOUDSMITH_API_BASE", base),
        );
        ctx.artifacts.add(Artifact {
            kind,
            name: art_name.to_string(),
            path,
            target: None,
            crate_name: "app".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx
    }

    /// Convenience: count log entries whose `(method, path)` exactly match.
    fn count_calls(log: &[RequestLog], method: &str, path: &str) -> usize {
        log.iter()
            .filter(|e| e.method == method && e.path == path)
            .count()
    }

    /// A files/create JSON response pointing step 2 back at this responder.
    fn files_create_ok(base: &str, id: &str) -> String {
        http_json(&format!(
            r#"{{"identifier":"{id}","upload_url":"{base}/s3-presigned/","upload_fields":{{"key":"v"}}}}"#
        ))
    }

    /// End-to-end happy path for a `.deb`: files/create -> presigned ->
    /// packages/upload/deb/. Asserts the step-3 URL routes to `/upload/deb/`,
    /// the step-1 body carries the md5 + filename, the step-3 body carries
    /// the files/create `identifier` and the configured `distribution`, and
    /// the returned target captures the response `slug_perm`.
    #[test]
    #[serial_test::serial]
    fn live_deb_full_three_step_records_slug_and_routes_deb() {
        use anodizer_core::config::CloudSmithDistributions;

        let tmp = tempfile::tempdir().unwrap();
        let art = tmp.path().join("app_1.0.0_amd64.deb");
        std::fs::write(&art, b"deb-bytes").unwrap();
        let md5 = md5_hex_of(b"deb-bytes");

        let (addr, log) = spawn_scripted_responder_with(move |addr| {
            let base = format!("http://{addr}");
            vec![
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/files/myorg/myrepo/",
                    response: Box::leak(files_create_ok(&base, "id-1").into_boxed_str()),
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/s3-presigned/",
                    response: PRESIGNED_204,
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/packages/myorg/myrepo/upload/deb/",
                    response: Box::leak(http_json(r#"{"slug_perm":"deb-slug"}"#).into_boxed_str()),
                    times: None,
                },
            ]
        });
        let base = format!("http://{addr}");

        let mut distros: HashMap<String, CloudSmithDistributions> = HashMap::new();
        distros.insert(
            "deb".to_string(),
            CloudSmithDistributions::Single("ubuntu/focal".to_string()),
        );
        let cfg = CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            distributions: Some(distros),
            republish: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };
        let ctx = ctx_with_one_artifact(
            cfg,
            &base,
            ArtifactKind::LinuxPackage,
            "app_1.0.0_amd64.deb",
            art.clone(),
            None,
        );

        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("ANODIZE_CLOUDSMITH_API_BASE", &base) };
        let result = publish_to_cloudsmith(&ctx, &StageLogger::new("cloudsmith", Verbosity::Quiet));
        unsafe { std::env::remove_var("ANODIZE_CLOUDSMITH_API_BASE") };
        drop(_g);

        let uploaded = result.expect("happy-path upload");
        assert_eq!(uploaded.len(), 1);
        assert_eq!(uploaded[0].slug.as_deref(), Some("deb-slug"));
        assert_eq!(uploaded[0].filename, "app_1.0.0_amd64.deb");

        let entries = log.lock().unwrap();
        assert_eq!(
            count_calls(&entries, "POST", "/files/myorg/myrepo/"),
            1,
            "exactly one files/create"
        );
        assert_eq!(
            count_calls(&entries, "POST", "/packages/myorg/myrepo/upload/deb/"),
            1,
            "step 3 routed to /upload/deb/"
        );
        let create = entries
            .iter()
            .find(|e| e.path == "/files/myorg/myrepo/")
            .unwrap();
        assert!(
            create.body.contains(&format!("\"md5_checksum\":\"{md5}\"")),
            "files/create body carries local md5: {}",
            create.body
        );
        assert!(
            create.body.contains("\"filename\":\"app_1.0.0_amd64.deb\""),
            "files/create body carries filename: {}",
            create.body
        );
        let step3 = entries
            .iter()
            .find(|e| e.path == "/packages/myorg/myrepo/upload/deb/")
            .unwrap();
        assert!(
            step3.body.contains("\"package_file\":\"id-1\""),
            "step-3 body threads the files/create identifier: {}",
            step3.body
        );
        assert!(
            step3.body.contains("\"distribution\":\"ubuntu/focal\""),
            "step-3 body carries the configured distribution: {}",
            step3.body
        );
    }

    /// Drive one artifact of `kind`/`art_name` through the 3-step flow with a
    /// responder whose step-3 route is `expected_step3_path`. Returns the
    /// captured request log so per-format tests can assert routing + body.
    /// `republish=true` so the pre-check packages-list query is skipped and
    /// the route table is exactly the 3 upload calls.
    fn run_one_format(
        art_name: &'static str,
        kind: ArtifactKind,
        expected_step3_path: &'static str,
        extra_cfg: impl FnOnce(&mut CloudSmithConfig),
    ) -> Vec<RequestLog> {
        let tmp = tempfile::tempdir().unwrap();
        let art = tmp.path().join(art_name);
        std::fs::write(&art, b"bytes").unwrap();

        let (addr, log) = spawn_scripted_responder_with(move |addr| {
            let base = format!("http://{addr}");
            vec![
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/files/myorg/myrepo/",
                    response: Box::leak(files_create_ok(&base, "id-f").into_boxed_str()),
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/s3-presigned/",
                    response: PRESIGNED_204,
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: expected_step3_path,
                    response: Box::leak(http_json(r#"{"slug_perm":"s"}"#).into_boxed_str()),
                    times: None,
                },
            ]
        });
        let base = format!("http://{addr}");

        let mut cfg = CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            // Match every format so the single artifact always passes the
            // filter. `zip` is included so a non-package Archive (which
            // `detect_format` slugs as `raw`) still clears the extension
            // filter — there is no literal `.raw` extension to match on.
            formats: Some(vec![
                "deb".to_string(),
                "rpm".to_string(),
                "srpm".to_string(),
                "apk".to_string(),
                "zip".to_string(),
            ]),
            republish: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };
        extra_cfg(&mut cfg);
        let ctx = ctx_with_one_artifact(cfg, &base, kind, art_name, art.clone(), None);

        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("ANODIZE_CLOUDSMITH_API_BASE", &base) };
        let result = publish_to_cloudsmith(&ctx, &StageLogger::new("cloudsmith", Verbosity::Quiet));
        unsafe { std::env::remove_var("ANODIZE_CLOUDSMITH_API_BASE") };
        drop(_g);
        result.expect("upload should succeed");
        let entries = log.lock().unwrap();
        entries.clone()
    }

    /// `.rpm` routes step 3 to `/upload/rpm/`.
    #[test]
    #[serial_test::serial]
    fn live_rpm_routes_to_upload_rpm() {
        let log = run_one_format(
            "app-1.0.0.x86_64.rpm",
            ArtifactKind::LinuxPackage,
            "/packages/myorg/myrepo/upload/rpm/",
            |_| {},
        );
        assert_eq!(
            count_calls(&log, "POST", "/packages/myorg/myrepo/upload/rpm/"),
            1,
        );
    }

    /// `.src.rpm` is a distinct format slug: step 3 routes to `/upload/srpm/`,
    /// NOT `/upload/rpm/` (the suffix overlap is resolved in `detect_format`).
    #[test]
    #[serial_test::serial]
    fn live_src_rpm_routes_to_upload_srpm() {
        let log = run_one_format(
            "app-1.0.0.src.rpm",
            ArtifactKind::LinuxPackage,
            "/packages/myorg/myrepo/upload/srpm/",
            |_| {},
        );
        assert_eq!(
            count_calls(&log, "POST", "/packages/myorg/myrepo/upload/srpm/"),
            1,
        );
        assert_eq!(
            count_calls(&log, "POST", "/packages/myorg/myrepo/upload/rpm/"),
            0,
            "src.rpm must not route to the rpm slug",
        );
    }

    /// `.apk` maps to the API-side `alpine` slug: step 3 -> `/upload/alpine/`.
    #[test]
    #[serial_test::serial]
    fn live_apk_routes_to_upload_alpine() {
        let log = run_one_format(
            "app-1.0.0.apk",
            ArtifactKind::LinuxPackage,
            "/packages/myorg/myrepo/upload/alpine/",
            |_| {},
        );
        assert_eq!(
            count_calls(&log, "POST", "/packages/myorg/myrepo/upload/alpine/"),
            1,
        );
    }

    /// A non-package Archive (`.zip`) detects as the `raw` format and routes
    /// step 3 to `/upload/raw/` (the `detect_format` fallback slug).
    #[test]
    #[serial_test::serial]
    fn live_zip_archive_routes_to_upload_raw() {
        let log = run_one_format(
            "app-1.0.0.zip",
            ArtifactKind::Archive,
            "/packages/myorg/myrepo/upload/raw/",
            |_| {},
        );
        assert_eq!(
            count_calls(&log, "POST", "/packages/myorg/myrepo/upload/raw/"),
            1,
        );
    }

    /// `component:` is included in the step-3 body for `deb` (a
    /// component-bearing format).
    #[test]
    #[serial_test::serial]
    fn live_deb_includes_component_in_body() {
        let log = run_one_format(
            "app_1.0.0_amd64.deb",
            ArtifactKind::LinuxPackage,
            "/packages/myorg/myrepo/upload/deb/",
            |cfg| cfg.component = Some("contrib".to_string()),
        );
        let step3 = log
            .iter()
            .find(|e| e.path == "/packages/myorg/myrepo/upload/deb/")
            .unwrap();
        assert!(
            step3.body.contains("\"component\":\"contrib\""),
            "deb step-3 body carries component: {}",
            step3.body
        );
    }

    /// `component:` is DROPPED from the step-3 body for `rpm` (rpm is not in
    /// `COMPONENT_BEARING_FORMATS`); the upload still succeeds.
    #[test]
    #[serial_test::serial]
    fn live_rpm_drops_component_from_body() {
        let log = run_one_format(
            "app-1.0.0.x86_64.rpm",
            ArtifactKind::LinuxPackage,
            "/packages/myorg/myrepo/upload/rpm/",
            |cfg| cfg.component = Some("contrib".to_string()),
        );
        let step3 = log
            .iter()
            .find(|e| e.path == "/packages/myorg/myrepo/upload/rpm/")
            .unwrap();
        assert!(
            !step3.body.contains("component"),
            "rpm step-3 body must not carry a component: {}",
            step3.body
        );
    }

    /// `republish: true` puts `"republish": true` into the step-3 body so
    /// Cloudsmith overwrites an existing package rather than 409ing.
    #[test]
    #[serial_test::serial]
    fn live_republish_sets_republish_flag_in_body() {
        let log = run_one_format(
            "app_1.0.0_amd64.deb",
            ArtifactKind::LinuxPackage,
            "/packages/myorg/myrepo/upload/deb/",
            |_| {},
        );
        let step3 = log
            .iter()
            .find(|e| e.path == "/packages/myorg/myrepo/upload/deb/")
            .unwrap();
        assert!(
            step3.body.contains("\"republish\":true"),
            "step-3 body carries republish flag: {}",
            step3.body
        );
    }

    /// Run a publish for a single `.deb` artifact against a caller-supplied
    /// route table (built once the responder addr is known), returning the
    /// publish `Result` and the request log. `cfg_mut` customizes the entry;
    /// `retry` lets retry-path tests inject a fast policy.
    #[allow(clippy::type_complexity)]
    fn run_deb_with_routes<R>(
        routes_fn: R,
        cfg_mut: impl FnOnce(&mut CloudSmithConfig),
        retry: Option<RetryConfig>,
    ) -> (Result<Vec<CloudsmithTarget>>, Vec<RequestLog>)
    where
        R: FnOnce(&str) -> Vec<ScriptedRoute> + Send + 'static,
    {
        let tmp = tempfile::tempdir().unwrap();
        let art = tmp.path().join("app_1.0.0_amd64.deb");
        std::fs::write(&art, b"deb-bytes").unwrap();

        let (addr, log) = spawn_scripted_responder_with(move |addr| {
            let base = format!("http://{addr}");
            routes_fn(&base)
        });
        let base = format!("http://{addr}");

        let mut cfg = CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            formats: Some(vec!["deb".to_string()]),
            ..Default::default()
        };
        cfg_mut(&mut cfg);
        let ctx = ctx_with_one_artifact(
            cfg,
            &base,
            ArtifactKind::LinuxPackage,
            "app_1.0.0_amd64.deb",
            art.clone(),
            retry,
        );

        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("ANODIZE_CLOUDSMITH_API_BASE", &base) };
        let result = publish_to_cloudsmith(&ctx, &StageLogger::new("cloudsmith", Verbosity::Quiet));
        unsafe { std::env::remove_var("ANODIZE_CLOUDSMITH_API_BASE") };
        drop(_g);
        let entries = log.lock().unwrap().clone();
        (result, entries)
    }

    /// The exact pre-check packages-list path reqwest produces for
    /// `filename:app_1.0.0_amd64.deb` (the colon percent-encodes to `%3A`;
    /// dots/underscores are unreserved). Routing the GET here lets the
    /// republish=false pre-check be driven without a real Cloudsmith.
    const PRECHECK_PATH: &str =
        "/packages/myorg/myrepo/?query=filename%3Aapp_1.0.0_amd64.deb&page_size=100";

    fn precheck_route(body: &'static str) -> ScriptedRoute {
        ScriptedRoute {
            method: "GET",
            path_pattern: PRECHECK_PATH,
            response: Box::leak(http_json(body).into_boxed_str()),
            times: None,
        }
    }

    /// republish=false + a pre-check that reports the same md5 ⇒ the upload
    /// is skipped (idempotent): no files/create, no step-3, empty targets.
    #[test]
    #[serial_test::serial]
    fn live_precheck_skip_idempotent_when_md5_matches() {
        let md5 = md5_hex_of(b"deb-bytes");
        let body: &'static str = Box::leak(
            format!(r#"[{{"filename":"app_1.0.0_amd64.deb","checksum_md5":"{md5}"}}]"#)
                .into_boxed_str(),
        );
        let (result, log) =
            run_deb_with_routes(move |_base| vec![precheck_route(body)], |_| {}, None);
        let uploaded = result.expect("idempotent skip is success");
        assert!(uploaded.is_empty(), "skip records no target: {uploaded:?}");
        assert_eq!(
            count_calls(&log, "POST", "/files/myorg/myrepo/"),
            0,
            "idempotent skip must not stage a file"
        );
        assert_eq!(count_calls(&log, "GET", PRECHECK_PATH), 1);
    }

    /// republish=false + a pre-check reporting a DIFFERENT md5 ⇒ bail with a
    /// conflict error naming both md5s; nothing is uploaded.
    #[test]
    #[serial_test::serial]
    fn live_precheck_bails_on_md5_mismatch() {
        let (result, log) = run_deb_with_routes(
            move |_base| {
                vec![precheck_route(
                    r#"[{"filename":"app_1.0.0_amd64.deb","checksum_md5":"00bad00remote"}]"#,
                )]
            },
            |_| {},
            None,
        );
        let err = result.expect_err("md5 mismatch must bail").to_string();
        assert!(err.contains("different md5"), "conflict error: {err}");
        assert!(err.contains("00bad00remote"), "names remote md5: {err}");
        assert_eq!(
            count_calls(&log, "POST", "/files/myorg/myrepo/"),
            0,
            "mismatch must not stage a file"
        );
    }

    /// A files/create that 4xxs surfaces the HTTP status and the response
    /// body in the error chain (and does not retry — 4xx fast-fails).
    #[test]
    #[serial_test::serial]
    fn live_files_create_4xx_surfaces_body() {
        let (result, log) = run_deb_with_routes(
            move |_base| {
                vec![ScriptedRoute {
                    method: "POST",
                    path_pattern: "/files/myorg/myrepo/",
                    response: "HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 24\r\n\r\n{\"detail\":\"bad md5 sum\"}",
                    times: None,
                }]
            },
            |cfg| cfg.republish = Some(StringOrBool::Bool(true)),
            None,
        );
        let err = format!("{:#}", result.expect_err("4xx must error"));
        assert!(err.contains("422"), "status in error: {err}");
        assert!(err.contains("bad md5 sum"), "body in error: {err}");
        assert_eq!(
            count_calls(&log, "POST", "/files/myorg/myrepo/"),
            1,
            "4xx must NOT retry"
        );
    }

    /// A files/create 200 whose JSON lacks `identifier` is a contract
    /// violation: bail with a message naming the missing field + artifact.
    #[test]
    #[serial_test::serial]
    fn live_files_create_missing_identifier_errors() {
        let (result, _log) = run_deb_with_routes(
            move |base| {
                vec![ScriptedRoute {
                    method: "POST",
                    path_pattern: "/files/myorg/myrepo/",
                    response: Box::leak(
                        http_json(&format!(
                            r#"{{"upload_url":"{base}/s3/","upload_fields":{{}}}}"#
                        ))
                        .into_boxed_str(),
                    ),
                    times: None,
                }]
            },
            |cfg| cfg.republish = Some(StringOrBool::Bool(true)),
            None,
        );
        let err = result
            .expect_err("missing identifier must bail")
            .to_string();
        assert!(err.contains("identifier"), "names missing field: {err}");
        assert!(err.contains("app_1.0.0_amd64.deb"), "names artifact: {err}");
    }

    /// A files/create 200 missing `upload_url` bails naming that field.
    #[test]
    #[serial_test::serial]
    fn live_files_create_missing_upload_url_errors() {
        let (result, _log) = run_deb_with_routes(
            move |_base| {
                vec![ScriptedRoute {
                    method: "POST",
                    path_pattern: "/files/myorg/myrepo/",
                    response: Box::leak(
                        http_json(r#"{"identifier":"id-1","upload_fields":{}}"#).into_boxed_str(),
                    ),
                    times: None,
                }]
            },
            |cfg| cfg.republish = Some(StringOrBool::Bool(true)),
            None,
        );
        let err = result
            .expect_err("missing upload_url must bail")
            .to_string();
        assert!(err.contains("upload_url"), "names missing field: {err}");
    }

    /// A files/create that returns a 200 with a non-JSON body bails with the
    /// "non-JSON body" diagnostic (the parse-context branch).
    #[test]
    #[serial_test::serial]
    fn live_files_create_non_json_errors() {
        let (result, _log) = run_deb_with_routes(
            move |_base| {
                vec![ScriptedRoute {
                    method: "POST",
                    path_pattern: "/files/myorg/myrepo/",
                    response: "HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nnot-jsn",
                    times: None,
                }]
            },
            |cfg| cfg.republish = Some(StringOrBool::Bool(true)),
            None,
        );
        let err = result.expect_err("non-JSON must bail").to_string();
        assert!(err.contains("non-JSON body"), "diagnostic: {err}");
    }

    /// A 5xx on files/create retries (fast policy, 2 attempts allowed) and
    /// then succeeds on the 2nd attempt — the `times`-capped 500 route is
    /// exhausted, so the unlimited 200 route serves attempt 2. Two recorded
    /// files/create calls prove the retry actually happened.
    #[test]
    #[serial_test::serial]
    fn live_files_create_5xx_then_success_retries() {
        let (result, log) = run_deb_with_routes(
            move |base| {
                let base = base.to_string();
                vec![
                    // First attempt: 500 (capped to one hit).
                    ScriptedRoute {
                        method: "POST",
                        path_pattern: "/files/myorg/myrepo/",
                        response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 3\r\n\r\nerr",
                        times: Some(1),
                    },
                    // Second attempt: the 500 route is spent, so this matches.
                    ScriptedRoute {
                        method: "POST",
                        path_pattern: "/files/myorg/myrepo/",
                        response: Box::leak(files_create_ok(&base, "id-r").into_boxed_str()),
                        times: None,
                    },
                    ScriptedRoute {
                        method: "POST",
                        path_pattern: "/s3-presigned/",
                        response: PRESIGNED_204,
                        times: None,
                    },
                    ScriptedRoute {
                        method: "POST",
                        path_pattern: "/packages/myorg/myrepo/upload/deb/",
                        response: Box::leak(http_json(r#"{"slug_perm":"s"}"#).into_boxed_str()),
                        times: None,
                    },
                ]
            },
            |cfg| cfg.republish = Some(StringOrBool::Bool(true)),
            Some(fast_retry_config()),
        );
        let uploaded = result.expect("retry then success");
        assert_eq!(uploaded.len(), 1);
        assert_eq!(
            count_calls(&log, "POST", "/files/myorg/myrepo/"),
            2,
            "one 5xx + one success = two files/create attempts (retry fired)"
        );
    }

    /// A missing artifact file bails before any HTTP call.
    #[test]
    #[serial_test::serial]
    fn live_missing_artifact_file_bails() {
        let (addr, log) = spawn_scripted_responder_with(|_| Vec::new());
        let base = format!("http://{addr}");
        let cfg = CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            formats: Some(vec!["deb".to_string()]),
            republish: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };
        // Point the artifact at a path that does not exist.
        let ctx = ctx_with_one_artifact(
            cfg,
            &base,
            ArtifactKind::LinuxPackage,
            "app_1.0.0_amd64.deb",
            PathBuf::from("/nonexistent/app_1.0.0_amd64.deb"),
            None,
        );
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("ANODIZE_CLOUDSMITH_API_BASE", &base) };
        let result = publish_to_cloudsmith(&ctx, &StageLogger::new("cloudsmith", Verbosity::Quiet));
        unsafe { std::env::remove_var("ANODIZE_CLOUDSMITH_API_BASE") };
        drop(_g);
        let err = result.expect_err("missing file must bail").to_string();
        assert!(err.contains("artifact file not found"), "{err}");
        assert!(
            log.lock().unwrap().is_empty(),
            "no HTTP call before the file check"
        );
    }

    /// When no artifact matches the format filter, the publisher reports the
    /// no-match status and returns an empty target list (no HTTP traffic).
    #[test]
    #[serial_test::serial]
    fn live_no_matching_artifacts_is_noop() {
        // A `.rpm` artifact but a `deb`-only filter ⇒ zero matches.
        let log = {
            let (addr, log) = spawn_scripted_responder_with(|_| Vec::new());
            let base = format!("http://{addr}");
            let cfg = CloudSmithConfig {
                organization: Some("myorg".to_string()),
                repository: Some("myrepo".to_string()),
                formats: Some(vec!["deb".to_string()]),
                ..Default::default()
            };
            let tmp = tempfile::tempdir().unwrap();
            let art = tmp.path().join("app-1.0.0.x86_64.rpm");
            std::fs::write(&art, b"x").unwrap();
            let ctx = ctx_with_one_artifact(
                cfg,
                &base,
                ArtifactKind::LinuxPackage,
                "app-1.0.0.x86_64.rpm",
                art.clone(),
                None,
            );
            let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
            unsafe { std::env::set_var("ANODIZE_CLOUDSMITH_API_BASE", &base) };
            let result =
                publish_to_cloudsmith(&ctx, &StageLogger::new("cloudsmith", Verbosity::Quiet));
            unsafe { std::env::remove_var("ANODIZE_CLOUDSMITH_API_BASE") };
            drop(_g);
            assert!(result.expect("no-match is ok").is_empty());
            log
        };
        assert!(log.lock().unwrap().is_empty(), "no upload attempted");
    }

    /// step-3 response carrying only `slug` (not `slug_perm`) still captures
    /// the slug into the recorded target (the `or_else` fallback key).
    #[test]
    #[serial_test::serial]
    fn live_step3_slug_fallback_key() {
        let (result, _log) = run_deb_with_routes(
            move |base| {
                let base = base.to_string();
                vec![
                    ScriptedRoute {
                        method: "POST",
                        path_pattern: "/files/myorg/myrepo/",
                        response: Box::leak(files_create_ok(&base, "id-1").into_boxed_str()),
                        times: None,
                    },
                    ScriptedRoute {
                        method: "POST",
                        path_pattern: "/s3-presigned/",
                        response: PRESIGNED_204,
                        times: None,
                    },
                    ScriptedRoute {
                        method: "POST",
                        path_pattern: "/packages/myorg/myrepo/upload/deb/",
                        response: Box::leak(http_json(r#"{"slug":"plain-slug"}"#).into_boxed_str()),
                        times: None,
                    },
                ]
            },
            |cfg| cfg.republish = Some(StringOrBool::Bool(true)),
            None,
        );
        let uploaded = result.expect("upload ok");
        assert_eq!(uploaded[0].slug.as_deref(), Some("plain-slug"));
    }

    /// step-3 response with no recognizable slug field still records the
    /// target (slug = None) so the upload is counted; rollback degrades to
    /// the warn-only path for it.
    #[test]
    #[serial_test::serial]
    fn live_step3_no_slug_records_target_without_slug() {
        let (result, _log) = run_deb_with_routes(
            move |base| {
                let base = base.to_string();
                vec![
                    ScriptedRoute {
                        method: "POST",
                        path_pattern: "/files/myorg/myrepo/",
                        response: Box::leak(files_create_ok(&base, "id-1").into_boxed_str()),
                        times: None,
                    },
                    ScriptedRoute {
                        method: "POST",
                        path_pattern: "/s3-presigned/",
                        response: PRESIGNED_204,
                        times: None,
                    },
                    ScriptedRoute {
                        method: "POST",
                        path_pattern: "/packages/myorg/myrepo/upload/deb/",
                        response: Box::leak(http_json(r#"{"ok":true}"#).into_boxed_str()),
                        times: None,
                    },
                ]
            },
            |cfg| cfg.republish = Some(StringOrBool::Bool(true)),
            None,
        );
        let uploaded = result.expect("upload ok");
        assert_eq!(uploaded.len(), 1);
        assert!(uploaded[0].slug.is_none(), "no slug field ⇒ None");
        assert_eq!(uploaded[0].filename, "app_1.0.0_amd64.deb");
    }

    /// Build the (files/create, presigned, conflicting-step3) routes plus two
    /// sequenced pre-check GET routes: the FIRST GET (pre-check) returns `[]`
    /// (NotFound → proceed to upload); the SECOND GET (post-409 re-query)
    /// returns `recheck_body`. The step-3 route always 409s.
    fn conflict_recovery_routes(base: &str, recheck_body: &'static str) -> Vec<ScriptedRoute> {
        let base = base.to_string();
        vec![
            // Pre-check (republish=false): NotFound so the upload proceeds.
            ScriptedRoute {
                method: "GET",
                path_pattern: PRECHECK_PATH,
                response: Box::leak(http_json("[]").into_boxed_str()),
                times: Some(1),
            },
            // Post-409 re-query: the recovery verdict.
            ScriptedRoute {
                method: "GET",
                path_pattern: PRECHECK_PATH,
                response: Box::leak(http_json(recheck_body).into_boxed_str()),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/files/myorg/myrepo/",
                response: Box::leak(files_create_ok(&base, "id-1").into_boxed_str()),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/s3-presigned/",
                response: PRESIGNED_204,
                times: None,
            },
            // step-3 always conflicts (409). 4xx fast-fails (no retry), so a
            // single capped response is enough.
            ScriptedRoute {
                method: "POST",
                path_pattern: "/packages/myorg/myrepo/upload/deb/",
                response: "HTTP/1.1 409 Conflict\r\nContent-Length: 13\r\n\r\nalready there",
                times: None,
            },
        ]
    }

    /// step-3 409 + a re-query showing the same md5 already landed (a
    /// concurrent uploader won the race) ⇒ idempotent skip: Ok, no target
    /// recorded, and the recovery re-query actually fired (2 GETs).
    #[test]
    #[serial_test::serial]
    fn live_step3_409_recovers_as_idempotent_skip() {
        let md5 = md5_hex_of(b"deb-bytes");
        let recheck: &'static str = Box::leak(
            format!(r#"[{{"filename":"app_1.0.0_amd64.deb","checksum_md5":"{md5}"}}]"#)
                .into_boxed_str(),
        );
        let (result, log) = run_deb_with_routes(
            move |base| conflict_recovery_routes(base, recheck),
            |_| {},
            Some(fast_retry_config()),
        );
        let uploaded = result.expect("409 with matching remote md5 ⇒ idempotent skip");
        assert!(uploaded.is_empty(), "skip records no target: {uploaded:?}");
        assert_eq!(
            count_calls(&log, "GET", PRECHECK_PATH),
            2,
            "pre-check + post-409 recovery re-query"
        );
    }

    /// step-3 409 + a re-query showing a DIFFERENT md5 ⇒ surface the conflict
    /// (a concurrent uploader landed different bytes under our name).
    #[test]
    #[serial_test::serial]
    fn live_step3_409_recovery_bails_on_md5_mismatch() {
        let (result, _log) = run_deb_with_routes(
            move |base| {
                conflict_recovery_routes(
                    base,
                    r#"[{"filename":"app_1.0.0_amd64.deb","checksum_md5":"00different00"}]"#,
                )
            },
            |_| {},
            Some(fast_retry_config()),
        );
        let err = result
            .expect_err("409 + different remote md5 must bail")
            .to_string();
        assert!(
            err.contains("step-3 conflict"),
            "names step-3 conflict: {err}"
        );
        assert!(err.contains("00different00"), "names remote md5: {err}");
    }

    /// step-3 409 + a re-query showing the package is NOT present (the 409 was
    /// not a same-name race) ⇒ the original step-3 error re-propagates instead
    /// of being silently swallowed.
    #[test]
    #[serial_test::serial]
    fn live_step3_409_recovery_repropagates_when_not_found() {
        let (result, _log) = run_deb_with_routes(
            move |base| conflict_recovery_routes(base, "[]"),
            |_| {},
            Some(fast_retry_config()),
        );
        let err = format!("{:#}", result.expect_err("409 + still-absent must error"));
        assert!(err.contains("409"), "original 409 status propagates: {err}");
    }

    // ---- live keep_versions retention pruning (list + DELETE) ------------
    //
    // The prune path (`prune_cloudsmith_versions` → `list_cloudsmith_package_versions`
    // → DELETE) was previously only exercised through the pure selector
    // (`select_versions_to_prune`); these drive the real HTTP list+delete
    // against the scripted responder. The package name pruning scopes to is
    // captured from the step-3 response `name` field, so the upload routes
    // must return a `name` for the prune to fire.

    /// The exact list path reqwest produces for the prune `name:` query of
    /// package `app` on page 1. `name:` → `name%3A`; `page`/`page_size` are
    /// appended in builder order.
    const PRUNE_LIST_PATH: &str = "/packages/myorg/myrepo/?query=name%3Aapp&page=1&page_size=100";

    /// The three upload routes for a single `.deb` whose step-3 response
    /// carries `slug_perm` + `name:"app"` so `keep_versions` pruning can scope
    /// to that package. `republish=true` keeps the pre-check off the route
    /// table.
    fn upload_routes_with_name(base: &str, slug: &str, name: &str) -> Vec<ScriptedRoute> {
        let body: &'static str = Box::leak(
            http_json(&format!(r#"{{"slug_perm":"{slug}","name":"{name}"}}"#)).into_boxed_str(),
        );
        vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/files/myorg/myrepo/",
                response: Box::leak(files_create_ok(base, "id-u").into_boxed_str()),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/s3-presigned/",
                response: PRESIGNED_204,
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/packages/myorg/myrepo/upload/deb/",
                response: body,
                times: None,
            },
        ]
    }

    /// A 204 No Content for a prune DELETE.
    const DELETE_204: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";

    /// Run a `.deb` publish with `keep_versions: keep`, a current `Version`,
    /// and a caller-supplied route table (list + DELETE routes layered on the
    /// upload routes). Returns the request log.
    fn run_prune(
        keep: u32,
        version: &str,
        extra_routes: impl FnOnce(&str) -> Vec<ScriptedRoute> + Send + 'static,
    ) -> Vec<RequestLog> {
        let tmp = tempfile::tempdir().unwrap();
        let art = tmp.path().join("app_1.0.0_amd64.deb");
        std::fs::write(&art, b"deb-bytes").unwrap();

        let (addr, log) = spawn_scripted_responder_with(move |addr| {
            let base = format!("http://{addr}");
            let mut routes = upload_routes_with_name(&base, "current-slug", "app");
            routes.extend(extra_routes(&base));
            routes
        });
        let base = format!("http://{addr}");

        let cfg = CloudSmithConfig {
            organization: Some("myorg".to_string()),
            repository: Some("myrepo".to_string()),
            formats: Some(vec!["deb".to_string()]),
            republish: Some(StringOrBool::Bool(true)),
            keep_versions: Some(keep),
            ..Default::default()
        };
        let mut ctx = ctx_with_one_artifact(
            cfg,
            &base,
            ArtifactKind::LinuxPackage,
            "app_1.0.0_amd64.deb",
            art.clone(),
            Some(fast_retry_config()),
        );
        // The prune is gated on a known current version (an empty version
        // disables it to protect the just-uploaded release).
        ctx.template_vars_mut().set("Version", version);

        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("ANODIZE_CLOUDSMITH_API_BASE", &base) };
        let result = publish_to_cloudsmith(&ctx, &StageLogger::new("cloudsmith", Verbosity::Quiet));
        unsafe { std::env::remove_var("ANODIZE_CLOUDSMITH_API_BASE") };
        drop(_g);
        result.expect("upload + prune should succeed");
        let entries = log.lock().unwrap();
        entries.clone()
    }

    /// keep_versions=2 over a list of 3 distinct versions ⇒ the single oldest
    /// version's slug is DELETEd; the list query + delete are real HTTP, and
    /// the upload's own version is always retained.
    #[test]
    #[serial_test::serial]
    fn live_prune_lists_and_deletes_oldest_version() {
        let list_body: &'static str = Box::leak(
            http_json(
                r#"[
                    {"name":"app","slug_perm":"current-slug","version":"0.9.1","uploaded_at":"2026-06-14T00:00:00Z"},
                    {"name":"app","slug_perm":"s-090","version":"0.9.0","uploaded_at":"2026-06-01T00:00:00Z"},
                    {"name":"app","slug_perm":"s-080","version":"0.8.0","uploaded_at":"2026-05-01T00:00:00Z"}
                ]"#,
            )
            .into_boxed_str(),
        );
        let log = run_prune(2, "0.9.1", move |_base| {
            vec![
                ScriptedRoute {
                    method: "GET",
                    path_pattern: PRUNE_LIST_PATH,
                    response: list_body,
                    times: None,
                },
                ScriptedRoute {
                    method: "DELETE",
                    path_pattern: "/packages/myorg/myrepo/s-080/",
                    response: DELETE_204,
                    times: None,
                },
            ]
        });
        // The list query fired exactly once (single short page).
        assert_eq!(
            count_calls(&log, "GET", PRUNE_LIST_PATH),
            1,
            "prune lists the package's versions: {log:?}"
        );
        // Only the oldest (0.8.0) is deleted; 0.9.1 (current) + 0.9.0 kept.
        assert_eq!(
            count_calls(&log, "DELETE", "/packages/myorg/myrepo/s-080/"),
            1,
            "oldest version's slug is DELETEd: {log:?}"
        );
        assert_eq!(
            count_calls(&log, "DELETE", "/packages/myorg/myrepo/s-090/"),
            0,
            "second-newest is retained (within keep=2)"
        );
        assert_eq!(
            count_calls(&log, "DELETE", "/packages/myorg/myrepo/current-slug/"),
            0,
            "the just-uploaded current version is never pruned"
        );
        // The DELETE carries the `Authorization: token <secret>` header that
        // only header capture can observe.
        let del = log
            .iter()
            .find(|e| e.method == "DELETE")
            .expect("delete request recorded");
        assert_eq!(
            del.header("Authorization"),
            Some("token fake-token"),
            "prune DELETE carries the token auth header: {:?}",
            del.headers
        );
    }

    /// keep_versions pages through a >100-entry list: a full first page
    /// (100 entries, all the current version) then a short second page
    /// carrying the prunable old version. Two GETs prove pagination fired.
    #[test]
    #[serial_test::serial]
    fn live_prune_paginates_until_short_page() {
        // Page 1: exactly PAGE_SIZE (100) entries, all version 0.9.1
        // (current), each a distinct slug. A full page forces a page-2 fetch.
        let mut page1 = String::from("[");
        for i in 0..100 {
            if i > 0 {
                page1.push(',');
            }
            page1.push_str(&format!(
                r#"{{"name":"app","slug_perm":"cur-{i}","version":"0.9.1","uploaded_at":"2026-06-14T00:00:00Z"}}"#
            ));
        }
        page1.push(']');
        let page1_resp: &'static str = Box::leak(http_json(&page1).into_boxed_str());
        let page2_resp: &'static str = Box::leak(
            http_json(
                r#"[{"name":"app","slug_perm":"old-1","version":"0.5.0","uploaded_at":"2026-01-01T00:00:00Z"}]"#,
            )
            .into_boxed_str(),
        );

        let log = run_prune(1, "0.9.1", move |_base| {
            vec![
                ScriptedRoute {
                    method: "GET",
                    path_pattern: "/packages/myorg/myrepo/?query=name%3Aapp&page=1&page_size=100",
                    response: page1_resp,
                    times: None,
                },
                ScriptedRoute {
                    method: "GET",
                    path_pattern: "/packages/myorg/myrepo/?query=name%3Aapp&page=2&page_size=100",
                    response: page2_resp,
                    times: None,
                },
                ScriptedRoute {
                    method: "DELETE",
                    path_pattern: "/packages/myorg/myrepo/old-1/",
                    response: DELETE_204,
                    times: None,
                },
            ]
        });
        assert_eq!(
            count_calls(
                &log,
                "GET",
                "/packages/myorg/myrepo/?query=name%3Aapp&page=1&page_size=100"
            ),
            1,
            "page 1 fetched"
        );
        assert_eq!(
            count_calls(
                &log,
                "GET",
                "/packages/myorg/myrepo/?query=name%3Aapp&page=2&page_size=100"
            ),
            1,
            "full first page forces a page-2 fetch (pagination): {log:?}"
        );
        assert_eq!(
            count_calls(&log, "DELETE", "/packages/myorg/myrepo/old-1/"),
            1,
            "the old version found on page 2 is pruned"
        );
    }

    /// A prune-list 4xx is non-fatal by contract: the upload already
    /// succeeded, so the publish still returns Ok and NO DELETE is issued
    /// (the warn-and-continue branch).
    #[test]
    #[serial_test::serial]
    fn live_prune_list_4xx_is_nonfatal_and_deletes_nothing() {
        let log = run_prune(2, "0.9.1", move |_base| {
            vec![ScriptedRoute {
                method: "GET",
                path_pattern: PRUNE_LIST_PATH,
                response: "HTTP/1.1 403 Forbidden\r\nContent-Length: 11\r\n\r\nno read acl",
                times: None,
            }]
        });
        // The upload itself still landed (run_prune asserts Ok); the prune
        // list failed → nothing deleted.
        assert_eq!(count_calls(&log, "GET", PRUNE_LIST_PATH), 1);
        assert_eq!(
            log.iter().filter(|e| e.method == "DELETE").count(),
            0,
            "a failed list must not delete anything: {log:?}"
        );
    }

    /// A prune DELETE 4xx is counted as a failure but is STILL non-fatal:
    /// the publish returns Ok (the upload succeeded) while the delete failure
    /// only warns. Proves the destructive follow-up can never fail the stage.
    #[test]
    #[serial_test::serial]
    fn live_prune_delete_4xx_is_nonfatal() {
        let list_body: &'static str = Box::leak(
            http_json(
                r#"[
                    {"name":"app","slug_perm":"current-slug","version":"0.9.1","uploaded_at":"2026-06-14T00:00:00Z"},
                    {"name":"app","slug_perm":"s-070","version":"0.7.0","uploaded_at":"2026-04-01T00:00:00Z"}
                ]"#,
            )
            .into_boxed_str(),
        );
        let log = run_prune(1, "0.9.1", move |_base| {
            vec![
                ScriptedRoute {
                    method: "GET",
                    path_pattern: PRUNE_LIST_PATH,
                    response: list_body,
                    times: None,
                },
                ScriptedRoute {
                    method: "DELETE",
                    path_pattern: "/packages/myorg/myrepo/s-070/",
                    response: "HTTP/1.1 403 Forbidden\r\nContent-Length: 6\r\n\r\ndenied",
                    times: None,
                },
            ]
        });
        // run_prune already asserted the publish returned Ok despite the 403.
        assert_eq!(
            count_calls(&log, "DELETE", "/packages/myorg/myrepo/s-070/"),
            1,
            "the delete was attempted (and failed non-fatally): {log:?}"
        );
    }

    /// Templated `organization` / `repository` (the workspace-style path
    /// where org/repo come from context vars rather than literals) render
    /// before any URL is built: `{{ .ProjectName }}` resolves to the
    /// project name so the upload routes to `/files/app-org/app-repo/`.
    /// Top-level publishers like cloudsmith don't resolve per-crate config,
    /// but they DO render their config values against the active context —
    /// this pins that the rendered values reach the wire.
    #[test]
    #[serial_test::serial]
    fn live_templated_org_repo_render_into_request_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let art = tmp.path().join("app_1.0.0_amd64.deb");
        std::fs::write(&art, b"deb-bytes").unwrap();

        let (addr, log) = spawn_scripted_responder_with(move |addr| {
            let base = format!("http://{addr}");
            vec![
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/files/app-org/app-repo/",
                    response: Box::leak(files_create_ok(&base, "id-t").into_boxed_str()),
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/s3-presigned/",
                    response: PRESIGNED_204,
                    times: None,
                },
                ScriptedRoute {
                    method: "POST",
                    path_pattern: "/packages/app-org/app-repo/upload/deb/",
                    response: Box::leak(http_json(r#"{"slug_perm":"s"}"#).into_boxed_str()),
                    times: None,
                },
            ]
        });
        let base = format!("http://{addr}");

        let cfg = CloudSmithConfig {
            // `app` is the project name seeded by ctx_with_one_artifact.
            organization: Some("{{ .ProjectName }}-org".to_string()),
            repository: Some("{{ .ProjectName }}-repo".to_string()),
            formats: Some(vec!["deb".to_string()]),
            republish: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };
        let ctx = ctx_with_one_artifact(
            cfg,
            &base,
            ArtifactKind::LinuxPackage,
            "app_1.0.0_amd64.deb",
            art.clone(),
            None,
        );

        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("ANODIZE_CLOUDSMITH_API_BASE", &base) };
        let result = publish_to_cloudsmith(&ctx, &StageLogger::new("cloudsmith", Verbosity::Quiet));
        unsafe { std::env::remove_var("ANODIZE_CLOUDSMITH_API_BASE") };
        drop(_g);

        let uploaded = result.expect("templated org/repo upload ok");
        assert_eq!(uploaded.len(), 1);
        assert_eq!(uploaded[0].org, "app-org", "org rendered from template");
        assert_eq!(uploaded[0].repo, "app-repo", "repo rendered from template");

        let entries = log.lock().unwrap();
        assert_eq!(
            count_calls(&entries, "POST", "/files/app-org/app-repo/"),
            1,
            "files/create routed to the rendered org/repo: {entries:?}"
        );
        assert_eq!(
            count_calls(&entries, "POST", "/packages/app-org/app-repo/upload/deb/"),
            1,
            "step-3 routed to the rendered org/repo"
        );
    }

    /// The step-1 files/create, the step-3 packages/upload, and the pre-check
    /// list all carry the `Authorization: token <secret>` header — asserted
    /// on the wire via the responder's header capture (previously
    /// unobservable). Drives republish=false so the pre-check GET is on the
    /// route table too.
    #[test]
    #[serial_test::serial]
    fn live_auth_header_present_on_every_cloudsmith_call() {
        let (result, log) = run_deb_with_routes(
            move |base| {
                let base = base.to_string();
                vec![
                    precheck_route("[]"),
                    ScriptedRoute {
                        method: "POST",
                        path_pattern: "/files/myorg/myrepo/",
                        response: Box::leak(files_create_ok(&base, "id-a").into_boxed_str()),
                        times: None,
                    },
                    ScriptedRoute {
                        method: "POST",
                        path_pattern: "/s3-presigned/",
                        response: PRESIGNED_204,
                        times: None,
                    },
                    ScriptedRoute {
                        method: "POST",
                        path_pattern: "/packages/myorg/myrepo/upload/deb/",
                        response: Box::leak(http_json(r#"{"slug_perm":"s"}"#).into_boxed_str()),
                        times: None,
                    },
                ]
            },
            |_| {},
            None,
        );
        result.expect("upload ok");
        // The pre-check, files/create, and step-3 each go to the Cloudsmith
        // API and must carry the token auth header. The S3 presigned POST
        // must NOT (it's an unauthenticated AWS form post).
        for path in [
            PRECHECK_PATH,
            "/files/myorg/myrepo/",
            "/packages/myorg/myrepo/upload/deb/",
        ] {
            let req = log
                .iter()
                .find(|e| e.path == path)
                .unwrap_or_else(|| panic!("request to {path} recorded: {log:?}"));
            assert_eq!(
                req.header("Authorization"),
                Some("token fake-token"),
                "{path} must carry the cloudsmith token: {:?}",
                req.headers
            );
        }
        let s3 = log
            .iter()
            .find(|e| e.path == "/s3-presigned/")
            .expect("presigned post recorded");
        assert!(
            s3.header("Authorization").is_none(),
            "S3 presigned upload must NOT carry a cloudsmith auth header: {:?}",
            s3.headers
        );
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    #[test]
    fn cloudsmith_publisher_classification() {
        let p = CloudsmithPublisher::new();
        assert_eq!(p.name(), "cloudsmith");
        assert_eq!(p.group(), PublisherGroup::Assets);
        assert!(!p.required());
        assert_eq!(
            p.rollback_scope_needed(),
            Some("CLOUDSMITH_API_KEY package_delete")
        );
    }

    #[test]
    fn cloudsmith_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = CloudsmithPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn cloudsmith_rollback_warns_when_no_targets_recorded() {
        // Empty evidence drives rollback into the no-targets branch.
        // The capture pins that production actually invoked `log.warn`
        // with the helper-formatted message — a hand-constructed expected
        // string compared against the helper output would pass even if
        // the rollback body forgot the warn entirely.
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("cloudsmith");
        let p = CloudsmithPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("cloudsmith")
                && m.contains("upload targets")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }

    /// Important #4 — per-target warn message renders a real cleanup
    /// instruction (org/repo/filename), not a fake URL.
    #[test]
    fn cloudsmith_manual_cleanup_msg_is_actionable() {
        let target = CloudsmithTarget {
            org: "acme".to_string(),
            repo: "widget".to_string(),
            filename: "widget_1.0.0_amd64.deb".to_string(),
            slug: None,
        };
        let msg = cloudsmith_manual_cleanup_msg(&target);
        assert!(msg.contains("widget_1.0.0_amd64.deb"), "{msg}");
        assert!(msg.contains("acme/widget"), "{msg}");
        // The prior implementation rendered a `?filename=` URL — make
        // sure that shape can't sneak back in.
        assert!(!msg.contains("?filename="), "{msg}");
        assert!(!msg.contains("api.cloudsmith.io"), "{msg}");
    }

    /// Structured (org, repo, filename) tuples round-trip through
    /// PublishEvidence.extra so a future schema change cannot silently
    /// regress the rollback warn shape.
    #[test]
    fn cloudsmith_target_extra_roundtrips() {
        let targets = vec![
            CloudsmithTarget {
                org: "acme".to_string(),
                repo: "widget".to_string(),
                filename: "widget_1.0.0_amd64.deb".to_string(),
                slug: None,
            },
            CloudsmithTarget {
                org: "acme".to_string(),
                repo: "widget".to_string(),
                filename: "widget-1.0.0-1.x86_64.rpm".to_string(),
                slug: None,
            },
        ];
        let encoded = encode_cloudsmith_targets(&targets);
        let decoded = decode_cloudsmith_targets(&encoded);
        assert_eq!(decoded, targets);
    }

    // Slug captured at upload time round-trips through evidence so
    // rollback can issue real DELETEs. Also pins the wire-format key
    // for older anodize binaries decoding this evidence.
    #[test]
    fn cloudsmith_target_serde_roundtrip_with_slug() {
        let targets = vec![
            CloudsmithTarget {
                org: "acme".to_string(),
                repo: "widget".to_string(),
                filename: "widget_1.0.0_amd64.deb".to_string(),
                slug: Some("aBcD1234".to_string()),
            },
            CloudsmithTarget {
                org: "acme".to_string(),
                repo: "widget".to_string(),
                filename: "widget-1.0.0-1.x86_64.rpm".to_string(),
                slug: Some("xY9Z".to_string()),
            },
        ];
        let encoded = encode_cloudsmith_targets(&targets);
        let decoded = decode_cloudsmith_targets(&encoded);
        assert_eq!(decoded, targets);
        // Wire-format pin: serialize through evidence and inspect the
        // JSON to confirm the slug rides under the `cloudsmith_targets`
        // key (matches the pre-typed shape).
        let mut e = PublishEvidence::new("cloudsmith");
        e.extra = encoded;
        let s = serde_json::to_string(&e).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&s).expect("parse");
        let arr = v["extra"]["cloudsmith_targets"]
            .as_array()
            .expect("cloudsmith_targets array");
        let first = arr.first().expect("at least one entry");
        assert_eq!(first.get("slug").and_then(|s| s.as_str()), Some("aBcD1234"));
    }

    // Evidence written by versions before slug capture decodes with
    // `slug = None`, so rollback degrades cleanly to the warn-only
    // path. The snapshot's `#[serde(default)]` on `slug` powers this
    // wire-compat path.
    #[test]
    fn cloudsmith_target_decode_tolerates_missing_slug_field() {
        // Hand-rolled JSON matching the pre-slug-capture evidence shape
        // — wrapped in the `PublishEvidence` envelope so deserialization
        // exercises the same path live evidence files take.
        let raw = r#"{
            "schema_version": 1,
            "publisher": "cloudsmith",
            "artifact_paths": [],
            "extra": {
                "cloudsmith_targets": [
                    {
                        "org": "acme",
                        "repo": "widget",
                        "filename": "widget_1.0.0_amd64.deb"
                    },
                    {
                        "org": "acme",
                        "repo": "widget",
                        "filename": "widget-1.0.0-1.x86_64.rpm"
                    }
                ]
            }
        }"#;
        let e: PublishEvidence = serde_json::from_str(raw).expect("deserialize");
        let decoded = decode_cloudsmith_targets(&e.extra);
        assert_eq!(decoded.len(), 2);
        assert!(
            decoded.iter().all(|t| t.slug.is_none()),
            "expected all slugs to decode as None for older evidence"
        );
        assert_eq!(decoded[0].filename, "widget_1.0.0_amd64.deb");
        assert_eq!(decoded[1].filename, "widget-1.0.0-1.x86_64.rpm");
    }

    // `null` slug values (the explicit serde shape when
    // `Option<String>` is None) also decode to `slug = None`.
    #[test]
    fn cloudsmith_target_decode_tolerates_null_slug() {
        let raw = r#"{
            "schema_version": 1,
            "publisher": "cloudsmith",
            "artifact_paths": [],
            "extra": {
                "cloudsmith_targets": [
                    {
                        "org": "acme",
                        "repo": "widget",
                        "filename": "widget_1.0.0_amd64.deb",
                        "slug": null
                    }
                ]
            }
        }"#;
        let e: PublishEvidence = serde_json::from_str(raw).expect("deserialize");
        let decoded = decode_cloudsmith_targets(&e.extra);
        assert_eq!(decoded.len(), 1);
        assert!(decoded[0].slug.is_none());
    }

    #[test]
    fn cloudsmith_target_extra_carries_no_secret_material() {
        // Structural pin: build typed evidence and assert (a) no
        // credential-shaped keys appear AND (b) the operator-public
        // upload coordinates serialize.
        let mut e = PublishEvidence::new("cloudsmith");
        e.extra = encode_cloudsmith_targets(&[CloudsmithTarget {
            org: "acme".into(),
            repo: "widget".into(),
            filename: "widget_1.0.0_amd64.deb".into(),
            slug: Some("aBcD1234".into()),
        }]);
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"pat\":"), "{s}");
        assert!(!s.contains("\"auth\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        assert!(!s.contains("\"secret\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        // Positive shape: org/repo/filename + slug present.
        assert!(s.contains("\"org\":\"acme\""), "{s}");
        assert!(s.contains("\"repo\":\"widget\""), "{s}");
        assert!(s.contains("\"filename\":\"widget_1.0.0_amd64.deb\""), "{s}");
        assert!(s.contains("\"slug\":\"aBcD1234\""), "{s}");
    }

    // B13 — rollback against evidence whose targets all lack a slug
    // (older `--rollback-only --from-run` replays, or step-3 responses
    // that omitted the slug field) returns Ok and never tries to issue
    // a DELETE against the Cloudsmith API. The `CLOUDSMITH_API_KEY` is
    // also absent here to make doubly sure no network call fires.
    #[test]
    fn cloudsmith_rollback_falls_back_to_warn_when_slug_missing() {
        // Inject an empty env source so `CLOUDSMITH_API_KEY` resolves
        // unset regardless of the ambient process env; the warn-only
        // path is forced for both the no-slug AND no-token reasons.
        let mut ctx = TestContextBuilder::new().build();
        ctx.set_env_source(anodizer_core::MapEnvSource::new());
        let targets = vec![
            CloudsmithTarget {
                org: "acme".to_string(),
                repo: "widget".to_string(),
                filename: "widget_1.0.0_amd64.deb".to_string(),
                slug: None,
            },
            CloudsmithTarget {
                org: "acme".to_string(),
                repo: "widget".to_string(),
                filename: "widget-1.0.0-1.x86_64.rpm".to_string(),
                slug: None,
            },
        ];
        let mut evidence = PublishEvidence::new("cloudsmith");
        evidence.extra = encode_cloudsmith_targets(&targets);
        evidence.artifact_paths = targets
            .iter()
            .map(|t| std::path::PathBuf::from(format!("{}/{}/{}", t.org, t.repo, t.filename)))
            .collect();

        let p = CloudsmithPublisher::new();
        assert!(
            p.rollback(&mut ctx, &evidence).is_ok(),
            "rollback must return Ok in warn-only fallback"
        );

        // Pin the exact warn-line shape so a refactor of
        // `cloudsmith_manual_cleanup_msg` can't silently regress the
        // operator instructions.
        let msg = cloudsmith_manual_cleanup_msg(&targets[0]);
        assert!(msg.contains("widget_1.0.0_amd64.deb"), "{msg}");
        assert!(msg.contains("acme/widget"), "{msg}");
        assert!(msg.contains("per-package slug not surfaced"), "{msg}");
        assert!(msg.contains("Cloudsmith dashboard"), "{msg}");
    }
}
