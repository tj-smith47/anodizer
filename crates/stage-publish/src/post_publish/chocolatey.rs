//! Post-publish poller for Chocolatey community-repository moderation.
//!
//! Strategy: scrape the HTML page
//! `https://community.chocolatey.org/packages/<name>/<version>` for the
//! moderation-status callout. The OData API (`/api/v2/Packages(...)`)
//! does NOT surface the in-moderation visual state — it only emits
//! `<d:PackageStatus>` for approved/rejected rows, so the HTML page is
//! the canonical signal for "still in queue".
//!
//! HTML signals verified against live pages (2026-05-13):
//!
//! - **Approved**: `<div class="callout-header">Package Approved</div>`
//!   appearing inside a `callout-success` block.
//!   Reference: `https://community.chocolatey.org/packages/git/2.50.1`.
//!
//! - **In moderation**:
//!     - `<div class="callout callout-danger">` containing
//!       `<div class="callout-header">IMPORTANT</div>` and the literal text
//!       `This version is in <a ...>moderation</a> and has not yet been approved`.
//!     - `<div class="callout callout-warning">` containing
//!       `<div class="callout-header">WARNING</div>` and the literal text
//!       `awaiting moderation`.
//!
//!   Reference: `https://community.chocolatey.org/packages/anodizer/0.2.0`.
//!
//! - **Not yet indexed**: server returns `404 Not Found` for a freshly
//!   pushed version that hasn't been ingested. Treated as `Pending`
//!   throughout the poll budget — a just-submitted package sitting in
//!   the human-moderator queue is the expected, normal state on a
//!   single-shot release, and a chronic 404 there is not actionable for
//!   the operator (moderation queues can sit for days). If the page
//!   was previously observed as resolvable (any HTTP 200) in the same
//!   run and then flipped to `404`, that IS a regression — the package
//!   was delisted after appearing — and is promoted to `Error` with a
//!   warning.
//!
//! - **Rejected**: per docs, rejected pages are not publicly visible
//!   ("the maintainer will see a message, but no one else will see or be
//!   able to install the package"), so the public scraper only sees
//!   `404`. The OData-side `PackageStatus=Rejected` signal is already
//!   handled by [`crate::chocolatey::publish`] during the publish step
//!   itself, so we don't need to re-detect rejection here.

use std::time::{Duration, Instant};

use anodizer_core::http::{blocking_client, body_of_blocking};
use anodizer_core::log::StageLogger;

use crate::post_publish::sleep_or_timeout;
use crate::post_publish::status::PostPublishStatus;

use anodizer_core::config::PostPublishPollConfig;

/// Per-request HTTP timeout for a single HTML probe. The polling loop
/// has its own wall-clock budget (`cfg.timeout`); the request timeout
/// protects against a network blackhole stalling the whole poll for an
/// `interval` period.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Grace window after which the `404` diagnostic detail switches from
/// "page not yet indexed" to "still not indexed after <duration>" so a
/// debug operator reading verbose logs can tell at a glance how long
/// the page has been missing. The status remains `Pending` in either
/// case — moderation queues routinely sit for days, so a chronic `404`
/// on a freshly-submitted package is the expected state and not
/// promoted to `Error` unless the page was previously observed
/// resolvable (any HTTP 200) in the same run (regression detection).
const NOT_FOUND_GRACE_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Verdict of a single HTML scrape — either we resolved to a terminal
/// state, observed a pending state, or hit a transient/transport issue.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PageVerdict {
    Approved(String),
    Pending(String),
    NotFound,
    NetworkError(String),
}

/// Render the operator-facing approval line. `'{package}-{version}'` is
/// the package-version pair used throughout this module's messages; the
/// parenthetical carries the scraped page detail. Factored out so a test
/// can pin the argument order (a swapped detail/version renders garbage
/// like `approved 'mytool-approved, ready for install' (1.2.3)`).
fn approved_message(package: &str, version: &str, detail: &str) -> String {
    format!(
        "chocolatey moderation approved '{}-{}' ({})",
        package, version, detail
    )
}

/// Poll the Chocolatey community page until a terminal state is
/// reached or the polling budget is exhausted.
///
/// `page_base_url` is normally `https://community.chocolatey.org` —
/// exposed as a parameter so tests can point at a local TCP listener.
#[allow(unused_assignments)]
// initial `None` is overwritten by every match arm before the
// timeout exit reads `last_pending_detail`; the compiler cannot prove the
// loop body runs at least once, so the initial assignment triggers the lint.
pub fn poll(
    page_base_url: &str,
    package: &str,
    version: &str,
    cfg: PostPublishPollConfig,
    log: &StageLogger,
) -> PostPublishStatus {
    let url = format!(
        "{}/packages/{}/{}",
        page_base_url.trim_end_matches('/'),
        package,
        version
    );
    let interval = cfg.interval.duration();
    let total_budget = cfg.timeout.duration();
    let started = Instant::now();
    let mut not_found_since: Option<Instant> = None;
    let mut last_pending_detail: Option<String> = None;
    // Track whether the page was ever observed as resolvable (HTTP 200
    // with any classification) during this poll run. Distinguishes
    // "never-yet-visible" (expected initial state on a fresh submission
    // sitting in moderation — not actionable) from "was-visible-then-404"
    // (a regression: the package was delisted or rejected after
    // appearing, which IS actionable).
    let mut ever_visible = false;

    log.verbose(&format!(
        "polling chocolatey moderation at {} (interval={:?}, timeout={:?})",
        url, interval, total_budget
    ));

    loop {
        let elapsed = started.elapsed();
        let verdict = scrape_once(&url);
        match verdict {
            PageVerdict::Approved(detail) => {
                log.status(&approved_message(package, version, &detail));
                return PostPublishStatus::Approved { detail };
            }
            PageVerdict::Pending(detail) => {
                not_found_since = None;
                // Any HTTP 200 from a CDN edge — even a stale-cache
                // hit during an origin blip, or a page whose callout
                // text drifted upstream and fell through to the
                // catch-all "status callout not yet present" branch —
                // flips this. A subsequent legitimate `404` then
                // surfaces as a regression. Accepted trade-off:
                // false positives produce an investigable Error with
                // the URL, which is cheap to dismiss; false negatives
                // (suppressing a real takedown) are not.
                ever_visible = true;
                last_pending_detail = Some(detail.clone());
                log.verbose(&format!(
                    "chocolatey moderation pending for '{}-{}' — {} (polled {:?})",
                    package, version, detail, elapsed
                ));
            }
            PageVerdict::NotFound => {
                let nf_start = *not_found_since.get_or_insert_with(Instant::now);
                let nf_elapsed = nf_start.elapsed();
                if ever_visible {
                    // Regression: page resolved earlier in this run and
                    // now returns 404. Surfaces as Error so the operator
                    // sees the takedown.
                    let reason = format!(
                        "community.chocolatey.org returned 404 for {} after the page was \
                         previously visible in this run — package may have been delisted",
                        url
                    );
                    log.warn(&format!("chocolatey moderation halted — {}", reason));
                    return PostPublishStatus::Error { reason };
                }
                last_pending_detail = Some(if nf_elapsed >= NOT_FOUND_GRACE_WINDOW {
                    format!(
                        "page still not indexed after {:?} (HTTP 404 — likely awaiting moderation)",
                        nf_elapsed
                    )
                } else {
                    "page not yet indexed (HTTP 404)".to_string()
                });
                log.verbose(&format!(
                    "chocolatey moderation has not indexed '{}-{}' yet (404 for {:?})",
                    package, version, nf_elapsed
                ));
            }
            PageVerdict::NetworkError(msg) => {
                // Network error: the gallery was unreachable — cannot
                // distinguish a legitimate 404 ("not yet indexed") from an
                // outage. Reset the not-found timer to avoid over-counting
                // periods where the gallery was unreachable rather than
                // genuinely returning 404.
                not_found_since = None;
                last_pending_detail = Some(format!("transient network error: {}", msg));
                log.verbose(&format!(
                    "chocolatey moderation transient error scraping {}: {}",
                    url, msg
                ));
            }
        }

        let elapsed_now = started.elapsed();
        if !sleep_or_timeout(elapsed_now, interval, total_budget) {
            let last_state = last_pending_detail
                .clone()
                .unwrap_or_else(|| "no terminal state observed".to_string());
            // Timeout-with-no-positive on chocolatey is the expected
            // outcome for a single-shot release whose package is still
            // sitting in the human-moderator queue (often multi-day).
            // Log verbose only; the Timeout return variant still
            // surfaces to the release summary so an operator who DOES
            // want to follow up can see it.
            log.verbose(&format!(
                "chocolatey moderation poll budget for '{}-{}' elapsed after {:?} (last state: {})",
                package, version, total_budget, last_state
            ));
            return PostPublishStatus::timeout(last_state, started.elapsed());
        }
    }
}

/// Single HTTP+parse round. Public-in-module so tests can drive the
/// HTML classifier directly.
fn scrape_once(url: &str) -> PageVerdict {
    let client = match blocking_client(REQUEST_TIMEOUT) {
        Ok(c) => c,
        Err(e) => return PageVerdict::NetworkError(e.to_string()),
    };
    let resp = match client.get(url).send() {
        Ok(r) => r,
        Err(e) => return PageVerdict::NetworkError(e.to_string()),
    };
    if resp.status().as_u16() == 404 {
        return PageVerdict::NotFound;
    }
    if !resp.status().is_success() {
        return PageVerdict::NetworkError(format!("HTTP {}", resp.status()));
    }
    let body = body_of_blocking(resp);
    classify_html(&body)
}

/// HTML classifier — pure, parameterizable, no IO. Tests pin the exact
/// substring rules used against live pages.
///
/// Search order matters because the page can carry mixed signals:
///
/// 1. **`callout-danger` "This version is in <a>moderation</a>"** —
///    version-scoped and definitive. When present, the version we're
///    looking at is in the queue regardless of any other markers.
///
/// 2. **`Package Approved`** (callout-header inside callout-success) —
///    also version-scoped: it lives on the version page only when
///    *this* version was approved. Wins over any package-wide warning
///    because it's the specific, terminal answer for the current URL.
///
/// 3. **`awaiting moderation`** (callout-warning) — package-wide:
///    chocolatey emits this string on EVERY version page whenever ANY
///    version of the package is pending (verified live against
///    `anodizer/0.2.0`: the warning sits on already-approved version
///    pages too while a newer version is in the queue). Only matches
///    when neither version-scoped marker fired — at that point we're a
///    freshly-submitted version with no version-scoped callout yet.
///
/// 4. **No marker** → default-safe to `Pending`. The next poll round
///    catches the eventual `Package Approved` callout.
fn classify_html(body: &str) -> PageVerdict {
    // (1) version-scoped pending — `callout-danger` "This version is in
    // moderation". Match the literal English text so a class-name
    // refactor on the chocolatey site doesn't silently misclassify.
    if body.contains("This version is in <a") && body.contains(">moderation</a>") {
        return PageVerdict::Pending(
            "in moderation queue (this version not yet approved)".to_string(),
        );
    }

    // (2) version-scoped approval — beats the package-wide warning that
    // may coexist on the page. Exact literal matches the verified live
    // page for `git/2.50.1`.
    if body.contains(r#"<div class="callout-header">Package Approved</div>"#) {
        return PageVerdict::Approved("Package Approved".to_string());
    }

    // (3) package-wide pending — `awaiting moderation` callout-warning.
    // Reached only when no version-scoped marker matched above; means
    // we're a freshly-submitted version whose own callout hasn't
    // rendered yet.
    if body.contains("awaiting moderation") {
        return PageVerdict::Pending("awaiting moderation".to_string());
    }

    // (4) No recognizable status block — treat as pending with a
    // diagnostic detail rather than guessing. The poller will keep
    // sampling; if the page eventually adds an Approved callout, the
    // next round catches it.
    PageVerdict::Pending(NO_CALLOUT_DETAIL.to_string())
}

/// Detail string of [`classify_html`]'s catch-all arm — a 200 page carrying
/// none of the verified moderation/approval callouts. Named so the burn
/// probe can tell "definite pending/approved evidence" apart from "page
/// resolved but says nothing recognizable" without re-parsing the body.
const NO_CALLOUT_DETAIL: &str = "status callout not yet present on page";

/// One-shot burn-detection probe: is `package@version` already known to the
/// Chocolatey community gallery in a state that blocks re-submitting the same
/// version?
///
/// Chocolatey's flat OData API (`/api/v2/Packages`) surfaces only APPROVED
/// versions — a submission sitting in the human-moderation queue is invisible
/// there yet still blocks a re-push of the same version. The HTML version
/// page is the one public signal that covers both states, so this probe
/// scrapes it, reusing the classifier the post-publish moderation poller
/// pins against live pages.
///
/// The probe refuses only on POSITIVE evidence:
///
/// - `Ok(Some(detail))` — the page carries a verified approved / in-moderation
///   callout; the version is consumed and a same-version re-cut cannot land
///   cleanly.
/// - `Ok(None)` — the version was never submitted: the gallery returns 404,
///   OR redirects away from the requested version page (redirects are NOT
///   followed — a 3xx means the exact `/<id>/<version>` page does not exist),
///   OR returns a 200 with no recognizable callout (uncertainty warns and
///   clears rather than blocking a legitimate recovery on scraper drift).
/// - `Err` — the gallery could not be consulted (transport failure, 5xx
///   after the shallow retry ladder). Callers taking destructive decisions
///   choose their own fail-open/fail-closed stance on this.
///
/// `page_base_url` is normally `https://community.chocolatey.org` — exposed
/// so tests can point at a local responder.
pub fn version_blocked_on_gallery(
    page_base_url: &str,
    package: &str,
    version: &str,
    log: &StageLogger,
) -> anyhow::Result<Option<String>> {
    use anodizer_core::http::blocking_client_no_redirect;
    use anodizer_core::retry::{SuccessClass, http_status};
    let url = format!(
        "{}/packages/{}/{}",
        page_base_url.trim_end_matches('/'),
        package,
        version
    );
    let client = blocking_client_no_redirect(REQUEST_TIMEOUT)?;
    let label = format!("chocolatey burn probe for '{package}-{version}'");
    // AllowRedirects + the non-following client hands 3xx to the match below
    // instead of retrying it as a failure status.
    let result = super::burn_probe_get(
        &label,
        &client,
        &url,
        |req| req,
        SuccessClass::AllowRedirects,
        |status, _| format!("community gallery returned {status} for {url}"),
        log,
    );
    match result {
        Ok((status, _)) if status.is_redirection() => {
            log.verbose(&format!(
                "gallery redirected away from {url} ({status}) — the version page does \
                 not exist"
            ));
            Ok(None)
        }
        Ok((_status, body)) => match classify_html(&body) {
            PageVerdict::Approved(d) => Ok(Some(format!("approved on the gallery ({d})"))),
            PageVerdict::Pending(d) if d == NO_CALLOUT_DETAIL => {
                log.warn(&format!(
                    "the gallery page for '{package}-{version}' resolved but carries no \
                     recognizable moderation callout ({url}); treating it as NOT blocking — \
                     verify manually before re-submitting the same version"
                ));
                Ok(None)
            }
            PageVerdict::Pending(d) => Ok(Some(format!("submitted, currently: {d}"))),
            // classify_html never returns the transport-layer variants (it
            // only sees response bodies); mirror the uncertain-200 stance
            // rather than panicking a destructive-decision path.
            PageVerdict::NotFound | PageVerdict::NetworkError(_) => {
                log.warn(&format!(
                    "unexpected classifier verdict for {url}; treating it as NOT blocking"
                ));
                Ok(None)
            }
        },
        Err(e) if http_status(&e) == 404 => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_message_renders_package_version_pair_then_detail() {
        assert_eq!(
            approved_message("mytool", "1.2.3", "approved, ready for install"),
            "chocolatey moderation approved 'mytool-1.2.3' (approved, ready for install)"
        );
    }

    #[test]
    fn classifies_approved_callout() {
        let html = r#"<html><body>
            <div id="status" class="callout callout-marker-none p-0 callout-success">
              <div class="callout-header">Package Approved</div>
              <p>This package was approved as a trusted package on 09 Jul 2025.</p>
            </div>
        </body></html>"#;
        assert_eq!(
            classify_html(html),
            PageVerdict::Approved("Package Approved".to_string())
        );
    }

    #[test]
    fn classifies_in_moderation_callout_danger() {
        // Pattern verified against live anodizer/0.2.0 page (2026-05-13).
        let html = r#"<html><body>
            <div class="callout callout-danger">
              <div class="callout-header">IMPORTANT</div>
              <p>This version is in <a href="https://docs.chocolatey.org/...">moderation</a> and has not yet been approved.</p>
            </div>
        </body></html>"#;
        match classify_html(html) {
            PageVerdict::Pending(reason) => assert!(
                reason.contains("in moderation"),
                "unexpected pending reason: {reason}"
            ),
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[test]
    fn classifies_awaiting_moderation_warning() {
        let html = r#"<html><body>
            <div class="callout callout-warning">
              <div class="callout-header">WARNING</div>
              <p>There are versions of this package awaiting moderation.</p>
            </div>
        </body></html>"#;
        match classify_html(html) {
            PageVerdict::Pending(reason) => assert!(
                reason.contains("awaiting moderation"),
                "unexpected reason: {reason}"
            ),
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[test]
    fn in_moderation_callout_wins_over_approved_callout() {
        // Defense-in-depth: if a page somehow carries both signals,
        // the in-moderation callout must win — false-positive Approved
        // would silently mark a still-pending package as live.
        let html = r#"
            <div class="callout callout-success">
              <div class="callout-header">Package Approved</div>
            </div>
            <div class="callout callout-danger">
              <div class="callout-header">IMPORTANT</div>
              <p>This version is in <a>moderation</a> and has not yet been approved.</p>
            </div>"#;
        match classify_html(html) {
            PageVerdict::Pending(_) => {}
            other => panic!("expected Pending (in-moderation must win), got {other:?}"),
        }
    }

    #[test]
    fn chocolatey_approved_page_with_package_wide_pending_warning_is_approved() {
        // Chocolatey emits the `awaiting moderation` warning on EVERY
        // version page of the package whenever ANY version is in the
        // queue — including pages of already-approved versions. The
        // version-scoped `Package Approved` callout must win over the
        // package-wide warning so a re-poll of a previously-approved
        // version doesn't false-negative to Pending.
        let html = r#"<html><body>
            <div class="callout callout-success">
              <div class="callout-header">Package Approved</div>
              <p>This package was approved as a trusted package on 09 Jul 2025.</p>
            </div>
            <div class="callout callout-warning">
              <div class="callout-header">WARNING</div>
              <p>There are versions of this package awaiting moderation.</p>
            </div>
        </body></html>"#;
        assert_eq!(
            classify_html(html),
            PageVerdict::Approved("Package Approved".to_string()),
            "version-scoped Package Approved must beat package-wide awaiting-moderation warning"
        );
    }

    #[test]
    fn classifies_no_callout_as_pending() {
        let html = "<html><body><p>nothing here</p></body></html>";
        match classify_html(html) {
            PageVerdict::Pending(_) => {}
            other => panic!("expected Pending, got {other:?}"),
        }
    }
}
