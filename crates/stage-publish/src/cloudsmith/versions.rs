use super::*;

/// Outcome of checking whether a package already exists on Cloudsmith.
/// Returned by [`check_cloudsmith_package_exists`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CloudsmithPackageState {
    /// No package found with the given filename: caller should upload.
    NotFound,
    /// Package found with a **verified** matching md5: caller should skip
    /// (idempotent re-run).
    SkipIdempotent,
    /// Package found with a different md5: caller should bail loudly.
    /// `remote` is the md5 reported by Cloudsmith.
    Md5Mismatch { remote: String },
    /// A package with this filename exists but Cloudsmith reported no
    /// `checksum_md5` to compare against (still syncing, partially landed, or
    /// a checksum-less format). Presence-by-filename is NOT proof the remote
    /// bytes match the local ones, so the caller must upload rather than
    /// skip-and-claim-match — mirroring artifactory's `Unknown` and blob's
    /// `None`. Any real duplicate surfaces from the upload (409) path itself.
    Unverifiable,
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
            // Package exists by filename but Cloudsmith reported no checksum to
            // verify against. Presence alone does NOT prove the remote bytes
            // match the local ones (a partial/still-syncing prior upload
            // reports an empty checksum), so this must NOT skip-and-claim a
            // match — the caller uploads and lets the 409 path resolve a true
            // duplicate.
            return Ok(CloudsmithPackageState::Unverifiable);
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
pub(crate) fn rank_distinct_versions_desc(
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
    deadline: Option<std::time::Instant>,
    log: &StageLogger,
) -> Result<CloudsmithPackageState> {
    log.verbose(&format!(
        "checking existing cloudsmith package for '{}' (query={})",
        art_name, query
    ));
    let result = retry_request("packages/list", art_name, policy, deadline, log, || {
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
