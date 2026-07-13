//! crates.io sparse-index and web-API probing: index URLs, publish-state
//! lookups, and index-propagation polling.

use super::*;

// ---------------------------------------------------------------------------
// poll_crates_io_index
// ---------------------------------------------------------------------------

/// Build the sparse index URL for a crate name (path segments based on length).
///
/// Crate names per cargo are restricted to ASCII alphanumerics plus `-`/`_`
/// (cargo reference: "Crate names ... must be ASCII"), so the byte slices
/// below are guaranteed to land on character boundaries. The debug_assert
/// makes the invariant load-bearing — any caller passing a non-ASCII name
/// would surface the violation in a debug build long before the slice
/// could panic at runtime.
pub(crate) fn sparse_index_url(crate_name: &str) -> String {
    format!("https://index.crates.io/{}", sparse_index_path(crate_name))
}

/// 403 bodies from crates.io that mean the token AUTHENTICATED but the
/// endpoint refused it for policy reasons — the auth pipeline resolves the
/// token before the `allow_token` / endpoint-scope checks, so these messages
/// are only reachable with a real, unexpired token. `/api/v1/me` became
/// cookie-only (first message); endpoint-scoped tokens hit the second. An
/// invalid token instead gets `authentication failed`, which stays a Blocker.
///
/// Accepting scope denials means a token scoped too narrowly to publish (or
/// scoped to a subset of workspace crates) passes this probe and fails at
/// `cargo publish` — the publish API rejects before anything lands, so the
/// failure is orderly, but per-crate-scoped tokens in a multi-crate workspace
/// can still land a subset before hitting an out-of-scope crate. That
/// trade-off is inherent to supporting least-privilege scoped tokens: the
/// probe cannot enumerate a token's scopes from any token-accessible endpoint.
pub(crate) const CRATES_IO_AUTHENTICATED_DENIALS: &[&str] = &[
    "this action can only be performed on the crates.io website",
    "this token does not have the required permissions to perform this action",
];

/// The crates.io web-API base for the token-validity probe (`/api/v1/me`).
///
/// Mirrors the sparse-index base override in [`published_on_crates_io`]:
/// integration tests drive the real binary across a process boundary, so an
/// env-routed base pointing at a local responder is the only way to keep the
/// live token probe hermetic there. Honored ONLY under `ANODIZE_TEST_HARNESS=1`
/// so no production run can point the credential probe at a friendly endpoint.
pub(crate) fn crates_io_api_base() -> String {
    match std::env::var("ANODIZER_TEST_CRATES_IO_API_BASE") {
        Ok(base) if std::env::var("ANODIZE_TEST_HARNESS").as_deref() == Ok("1") => {
            base.trim_end_matches('/').to_string()
        }
        _ => "https://crates.io".to_string(),
    }
}

/// The registry-relative sparse-index path for a crate (`1/a`, `2/ab`,
/// `3/a/abc`, `ab/cd/abcdef`), shared by [`sparse_index_url`] and the
/// test-harness index-base override in [`published_on_crates_io`] so the
/// sharding scheme exists exactly once.
fn sparse_index_path(crate_name: &str) -> String {
    debug_assert!(
        crate_name.is_ascii(),
        "cargo crate names must be ASCII; got {crate_name:?}"
    );
    let lower = crate_name.to_ascii_lowercase();
    match lower.len() {
        1 => format!("1/{}", lower),
        2 => format!("2/{}", lower),
        3 => format!("3/{}/{}", &lower[..1], lower),
        _ => format!("{}/{}/{}", &lower[..2], &lower[2..4], lower),
    }
}

/// Parse the crates.io sparse-index body (JSON-lines, one entry per
/// published version) and return the `cksum` for `version` when present.
///
/// - Returns `None` when no line matches the requested version.
/// - Returns `Some("")` when the version exists but the line is missing its
///   `cksum` field — caller must treat this as "version present, drift
///   undetectable" rather than "not published".
///
/// Extracted from `is_already_published` so the JSONL shape can be unit
/// tested without performing a network call to crates.io.
pub(crate) fn parse_index_cksum_for_version(body: &str, version: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let v = serde_json::from_str::<serde_json::Value>(line).ok()?;
        if v.get("vers")?.as_str()? != version {
            return None;
        }
        Some(
            v.get("cksum")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
        )
    })
}

/// Probe crates.io's sparse index for whether `name` at `version` is
/// published — the GLOBAL registry answer, independent of any single run's
/// evidence. `Ok(true)` = the version is live (burned — crates.io never
/// accepts the same version twice), `Ok(false)` = positively absent (index
/// 404 or version missing from the index body), `Err` = the index could not
/// be consulted (callers making destructive decisions must FAIL CLOSED on
/// this).
///
/// Public so failure-recovery tooling (`tag rollback`'s published-state
/// guard) reuses the same sparse-index client + JSONL parser the publish
/// stage trusts, instead of growing a second index parser.
pub fn published_on_crates_io(
    name: &str,
    version: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    log: &StageLogger,
) -> Result<bool> {
    // Test-harness index-base override, mirroring `--simulate-failure`'s env
    // gating: integration tests drive the real binary across a process
    // boundary, so an env-routed base pointing at a local responder is the
    // only way to keep this probe hermetic there. Honored ONLY under
    // ANODIZE_TEST_HARNESS=1 so no production run can point the
    // published-state guard at a friendly index.
    let url = match std::env::var("ANODIZER_TEST_CRATES_IO_INDEX_BASE") {
        Ok(base) if std::env::var("ANODIZE_TEST_HARNESS").as_deref() == Ok("1") => {
            format!("{}/{}", base.trim_end_matches('/'), sparse_index_path(name))
        }
        _ => sparse_index_url(name),
    };
    Ok(is_already_published_at(&url, name, version, policy, log)?.is_some())
}

/// Whether a crate's resolved `publish.cargo` block targets the default
/// crates.io registry, where the sparse-index cksum the content-vs-version
/// guard compares against is authoritative.
///
/// A custom `registry =`/`index =` points cargo at a different index, so the
/// crates.io cksum the guard fetched describes a DIFFERENT artifact (or none).
/// The content guard — and the already-published idempotency skip itself —
/// only hold against the registry actually being published to, so both are
/// disabled for non-crates.io targets and the publish is attempted (the
/// target registry's own server-side conflict handling governs idempotency).
///
/// Public for the same reason as [`published_on_crates_io`]: `tag rollback`'s
/// published-state guard must scope its crates.io probe with the same
/// judgment the publisher applies.
pub fn targets_crates_io(cfg: Option<&CargoPublishConfig>) -> bool {
    match cfg {
        None => true,
        Some(c) => c.registry.is_none() && c.index.is_none(),
    }
}

/// Poll the crates.io sparse index until `crate_name` at `version` appears or
/// the deadline (seconds) is exceeded.  Uses exponential back-off starting at
/// `INITIAL_POLL_DELAY`, capped at `MAX_POLL_DELAY`.
///
/// Returns `Ok(())` when the version is confirmed, `Err` on timeout.
pub(crate) fn poll_crates_io_index(
    crate_name: &str,
    version: &str,
    timeout_secs: u64,
    log: &StageLogger,
) -> Result<()> {
    use std::time::Duration;
    // First SLEEP is 1s, not 5s: the sparse index frequently propagates a
    // freshly-published crate within 1-2s, so a hard 5s floor wastes the
    // common case. The first probe is free (no wait), and the early backoff
    // doubles 1→2→4→8… capped at MAX_POLL_DELAY, so a slow-propagating index
    // still backs off promptly without hammering the endpoint.
    poll_crates_io_index_at(
        &sparse_index_url(crate_name),
        crate_name,
        version,
        timeout_secs,
        Duration::from_secs(1),
        log,
    )
}

/// Same as [`poll_crates_io_index`] but uses the supplied URL and initial
/// back-off instead of computing them. Lets tests point at a local TCP
/// responder and skip the production 5 s first delay.
pub(crate) fn poll_crates_io_index_at(
    url: &str,
    crate_name: &str,
    version: &str,
    timeout_secs: u64,
    initial_backoff: std::time::Duration,
    log: &StageLogger,
) -> Result<()> {
    use std::time::{Duration, Instant};

    const MAX_POLL_DELAY: Duration = Duration::from_secs(60);

    let start = Instant::now();
    let deadline = Duration::from_secs(timeout_secs);

    let client = anodizer_core::http::blocking_client(Duration::from_secs(10))
        .context("publish: build HTTP client for index polling")?;

    let mut backoff = initial_backoff;

    // Per-attempt logs go to `debug` — transient HTTP errors are the
    // normal shape of "the index hasn't propagated yet"; surfacing them
    // at `warn`/`error` floods normal release output. The terminal
    // timeout below escalates with a single bail!() carrying the same
    // context the per-attempt logs would have shown.
    loop {
        match client.get(url).send() {
            Ok(resp) if resp.status().is_success() => {
                let body = anodizer_core::http::body_of_blocking(resp);
                // Each line of the sparse index is a JSON object; parse and check vers field.
                if body.lines().any(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .ok()
                        .and_then(|v| v.get("vers")?.as_str().map(|s| s == version))
                        .unwrap_or(false)
                }) {
                    log.verbose(&format!(
                        "crates.io index confirmed {}-{}",
                        crate_name, version
                    ));
                    return Ok(());
                }
            }
            Ok(resp) => {
                log.debug(&format!(
                    "crates.io index returned {} for {}, retrying…",
                    resp.status(),
                    crate_name
                ));
            }
            Err(e) => {
                log.debug(&format!(
                    "HTTP error polling index for {}: {}",
                    crate_name, e
                ));
            }
        }

        if start.elapsed() >= deadline {
            anyhow::bail!(
                "publish: timed out waiting for {}-{} to appear in crates.io index \
                 (waited {} s)",
                crate_name,
                version,
                timeout_secs
            );
        }

        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(MAX_POLL_DELAY);
    }
}

/// Probe the sparse index for `(crate_name, version)` once. Returns
/// `Ok(true)` when the version line is present, `Ok(false)` for any
/// non-success status (treated as "not yet"), `Err` on transport
/// failures the caller should surface.
///
/// Uses the same blocking HTTP client + JSONL parser as
/// [`is_already_published_at`] — the wait-for-deps gate and the
/// already-published short-circuit query the same endpoint, so sharing
/// the parser keeps the two paths byte-identical.
pub(crate) fn probe_dep_on_index(
    client: &reqwest::blocking::Client,
    url: &str,
    version: &str,
) -> Result<bool> {
    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("publish: wait_for_workspace_deps GET {url}"))?;
    if !resp.status().is_success() {
        return Ok(false);
    }
    let body = anodizer_core::http::body_of_blocking(resp);
    Ok(parse_index_cksum_for_version(&body, version).is_some())
}
