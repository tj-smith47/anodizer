//! Post-publish poller for WinGet PRs.
//!
//! Strategy: locate the open PR in `microsoft/winget-pkgs` whose title
//! contains `<PackageIdentifier> <Version>` (anodizer's PR title format
//! is `"New version: <PackageIdentifier> version <Version>"`, which the
//! GitHub `in:title` operator matches on word independence). Then poll
//! `GET /repos/microsoft/winget-pkgs/pulls/<number>` until the PR
//! reaches a terminal state.
//!
//! Label vocabulary verified live against `microsoft/winget-pkgs`
//! (2026-05-13, page 1+2 of `GET /repos/.../labels?per_page=100`):
//!
//! - **Approved (terminal success)**:
//!   - `Moderator-Approved`  — final human approval label
//!   - `Validation-Completed` + `Azure-Pipeline-Passed` together — clean
//!     validation run (a sufficient terminal positive for an open PR
//!     that's about to merge; pairs with `merged: true` once the bot
//!     auto-merges)
//!   - PR `state: closed`, `merged: true` — terminal success
//!
//! - **Rejected (terminal failure)**:
//!   - PR `state: closed`, `merged: false` — bot or moderator rejection
//!   - Any of these labels: `Validation-*-Error`, `Validation-*-Failed`,
//!     `Validation-*-Mismatch`, `Internal-Error*`, `Manifest-*-Error`,
//!     `PullRequest-Error`, `Changes-Requested`, `Needs-CLA`,
//!     `Author-Not-Authorized`, `Author-Not-Verified`,
//!     `Binary-Validation-Error`, `Blocking-Issue`, `Hardware`. The
//!     specific label that fired is preserved in `Rejected::detail` so
//!     the operator can act on it.
//!
//! - **Pending**: anything else (open PR with no terminal labels,
//!   intermediate labels like `New-Manifest`, `In-PR`, `Needs-Attention`,
//!   `Validation-Retry`).

use std::time::{Duration, Instant};

use anodizer_core::http::{blocking_client, body_of_blocking};
use anodizer_core::log::StageLogger;
use serde_json::Value;

use crate::post_publish::sleep_or_timeout;
use crate::post_publish::status::PostPublishStatus;

use anodizer_core::config::PostPublishPollConfig;

/// Per-request HTTP timeout for a single GitHub API call. The polling
/// loop has its own wall-clock budget (`cfg.timeout`); this protects
/// against a hung connection burning a full poll interval.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Outcome of a single GitHub API round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PrVerdict {
    Approved(String),
    Rejected(String),
    Pending(String),
    SearchEmpty,
    NetworkError(String),
}

/// Poll the winget-pkgs PR for terminal state.
///
/// `api_base_url` is normally `https://api.github.com` — exposed so
/// tests can point at a local TCP listener.
#[allow(unused_assignments)] // `last_pending_detail = None` initializer is
// dead code once the loop's first iteration overwrites it.
pub fn poll(
    api_base_url: &str,
    package_identifier: &str,
    version: &str,
    token: Option<&str>,
    cfg: PostPublishPollConfig,
    log: &StageLogger,
) -> PostPublishStatus {
    let interval = cfg.interval.duration();
    let total_budget = cfg.timeout.duration();
    let started = Instant::now();
    let mut last_pending_detail: Option<String> = None;
    let mut pr_url: Option<String> = None;
    // Track whether a matching PR was ever located during this poll
    // run. Distinguishes "PR not visible yet" (expected initial state
    // while the upstream search index ingests a fresh PR — not
    // actionable for the operator) from "PR was found then disappeared"
    // (a regression: the PR was deleted/withdrawn after first appearing,
    // which IS actionable).
    let mut ever_found = false;

    log.verbose(&format!(
        "polling winget PR for {} {} (interval={:?}, timeout={:?})",
        package_identifier, version, interval, total_budget
    ));

    loop {
        // Cheap fast-path: once we've located the PR, re-hit the PR
        // endpoint directly. The first iteration (and any iteration
        // where we lost track of the PR) falls back to the search.
        let verdict = match pr_url.as_deref() {
            Some(url) => check_pr_at(url, token),
            None => match locate_pr(api_base_url, package_identifier, version, token) {
                Some(url) => {
                    pr_url = Some(url.clone());
                    check_pr_at(&url, token)
                }
                None => PrVerdict::SearchEmpty,
            },
        };

        match verdict {
            PrVerdict::Approved(detail) => {
                log.status(&format!(
                    "winget PR: {} {} approved ({})",
                    package_identifier, version, detail
                ));
                return PostPublishStatus::Approved { detail };
            }
            PrVerdict::Rejected(detail) => {
                log.status(&format!(
                    "winget PR: {} {} rejected ({})",
                    package_identifier, version, detail
                ));
                return PostPublishStatus::Rejected { detail };
            }
            PrVerdict::Pending(detail) => {
                // A single transient `total_count: 1` from GitHub
                // search (index lag, search-shard inconsistency)
                // followed by a `check_pr_at` returning Pending
                // flips this. A subsequent legitimate empty search
                // is then treated as a regression. Accepted
                // trade-off: false positives produce an
                // investigable Error naming the package/version;
                // false negatives (suppressing a real PR
                // withdrawal) are not.
                ever_found = true;
                last_pending_detail = Some(detail.clone());
                log.verbose(&format!(
                    "winget PR: {} {} pending — {}",
                    package_identifier, version, detail
                ));
            }
            PrVerdict::SearchEmpty => {
                if ever_found {
                    // Regression: a matching PR was visible earlier in
                    // this run and has now disappeared from search.
                    // Surfaces as Error so the operator sees the
                    // takedown / withdrawal.
                    let reason = format!(
                        "winget PR for '{} {}' was previously located but has now disappeared \
                         from search — PR may have been closed or deleted",
                        package_identifier, version
                    );
                    log.warn(&format!("winget PR: {}", reason));
                    return PostPublishStatus::Error { reason };
                }
                last_pending_detail = Some("no matching PR found yet".to_string());
                log.verbose(&format!(
                    "winget PR: no PR matching '{} {}' visible yet",
                    package_identifier, version
                ));
            }
            PrVerdict::NetworkError(msg) => {
                last_pending_detail = Some(format!("transient network error: {}", msg));
                log.verbose(&format!("winget PR: transient error: {}", msg));
                // Force re-search on next iteration in case the PR URL went stale.
                pr_url = None;
            }
        }

        let elapsed_now = started.elapsed();
        if !sleep_or_timeout(elapsed_now, interval, total_budget) {
            let last_state = last_pending_detail
                .clone()
                .unwrap_or_else(|| "no terminal state observed".to_string());
            // Timeout-with-no-positive on winget is the expected
            // outcome when the upstream validation pipeline is still
            // running at budget exhaustion — moderator review and
            // pipeline retries can stretch beyond a typical 30min
            // budget. Log verbose only; the Timeout return variant
            // still surfaces to the release summary for follow-up.
            log.verbose(&format!(
                "winget PR: {} {} poll budget elapsed after {:?} (last state: {})",
                package_identifier, version, total_budget, last_state
            ));
            return PostPublishStatus::timeout(last_state, started.elapsed());
        }
    }
}

/// Locate the open PR via `GET /search/issues`. Returns the
/// `pulls/<number>` API URL (not the `html_url`) so the polling loop
/// can hit the PR endpoint directly on subsequent iterations.
fn locate_pr(
    api_base_url: &str,
    package_identifier: &str,
    version: &str,
    token: Option<&str>,
) -> Option<String> {
    // is:pr (state-agnostic, since a freshly-merged PR is closed but
    // still our PR) — preflight uses `is:open` because it's a pre-check;
    // post-publish needs the closed-too view to detect merge / rejection.
    let query = format!(
        "repo:microsoft/winget-pkgs is:pr {} {} in:title",
        package_identifier, version
    );
    let encoded = percent_encode(&query);
    let url = format!(
        "{}/search/issues?q={}&per_page=1",
        api_base_url.trim_end_matches('/'),
        encoded
    );

    let body = match http_get_json(&url, token) {
        Ok(b) => b,
        Err(_) => return None,
    };
    let v: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return None,
    };
    let total = v.get("total_count").and_then(|n| n.as_u64()).unwrap_or(0);
    if total == 0 {
        return None;
    }
    let items = v.get("items")?.as_array()?;
    let first = items.first()?;
    // The search-issues response gives us `pull_request.url` (the API
    // URL) — preferred over `html_url` since we're about to hit the API
    // on it directly. Fall back to constructing the API URL from
    // `number` if needed.
    if let Some(pr_url) = first
        .get("pull_request")
        .and_then(|pr| pr.get("url"))
        .and_then(|u| u.as_str())
    {
        return Some(pr_url.to_string());
    }
    let number = first.get("number").and_then(|n| n.as_u64())?;
    Some(format!(
        "{}/repos/microsoft/winget-pkgs/pulls/{}",
        api_base_url.trim_end_matches('/'),
        number
    ))
}

fn check_pr_at(pr_api_url: &str, token: Option<&str>) -> PrVerdict {
    let body = match http_get_json(pr_api_url, token) {
        Ok(b) => b,
        Err(e) => return PrVerdict::NetworkError(e),
    };
    let v: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return PrVerdict::NetworkError(format!("malformed PR JSON: {}", e)),
    };
    classify_pr_json(&v)
}

/// Pure PR-state classifier — no IO. Tests pin the exact label vocabulary
/// observed in `microsoft/winget-pkgs`.
fn classify_pr_json(v: &Value) -> PrVerdict {
    let state = v.get("state").and_then(|s| s.as_str()).unwrap_or("");
    let merged = v.get("merged").and_then(|m| m.as_bool()).unwrap_or(false);
    let labels: Vec<String> = v
        .get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|lbl| lbl.get("name").and_then(|n| n.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // Terminal merge state wins outright.
    if state == "closed" && merged {
        return PrVerdict::Approved("PR merged".to_string());
    }
    if state == "closed" && !merged {
        // Use the most informative label as the rejection detail when
        // available, falling back to a generic "closed without merge".
        let detail = first_rejection_label(&labels)
            .map(|l| format!("PR closed without merge (label: {})", l))
            .unwrap_or_else(|| "PR closed without merge".to_string());
        return PrVerdict::Rejected(detail);
    }

    // Open PR with one of the known rejection labels.
    if let Some(reject_label) = first_rejection_label(&labels) {
        return PrVerdict::Rejected(reject_label);
    }

    // Open PR with a positive moderator approval label.
    if labels.iter().any(|l| l == "Moderator-Approved") {
        return PrVerdict::Approved("Moderator-Approved".to_string());
    }
    // Validation pipeline passed cleanly — the bot will normally
    // auto-merge after this, but if the PR sits open with this combo
    // it's effectively a terminal-success signal for our purposes.
    let validation_completed = labels.iter().any(|l| l == "Validation-Completed");
    let pipeline_passed = labels.iter().any(|l| l == "Azure-Pipeline-Passed");
    if validation_completed && pipeline_passed {
        return PrVerdict::Approved("Validation-Completed + Azure-Pipeline-Passed".to_string());
    }

    // Otherwise: open PR is still in flight.
    let detail = if labels.is_empty() {
        "open, no labels yet".to_string()
    } else {
        format!("open with labels: {}", labels.join(", "))
    };
    PrVerdict::Pending(detail)
}

/// Return the first rejection-tier label name found in `labels`, or
/// `None` if none matches. The "first" is the lexically-first as the
/// labels array is iterated — kept deterministic for testability rather
/// than picking by severity (a single rejection label is sufficient to
/// fail the PR; severity ordering is the operator's call).
fn first_rejection_label(labels: &[String]) -> Option<String> {
    labels.iter().find(|l| is_rejection_label(l)).cloned()
}

/// True if the label signals a terminal rejection / blocking failure.
///
/// Patterns verified against live `microsoft/winget-pkgs` labels:
/// `Validation-*-Error`, `Validation-*-Failed`, `Validation-*-Mismatch`,
/// `Internal-Error*`, `Manifest-*-Error`, plus the literal names
/// `Needs-CLA`, `Changes-Requested`, `Author-Not-Authorized`,
/// `Author-Not-Verified`, `Binary-Validation-Error`, `Blocking-Issue`,
/// `PullRequest-Error`.
fn is_rejection_label(label: &str) -> bool {
    // Literal-name matches first (cheaper than substring scans).
    matches!(
        label,
        "Needs-CLA"
            | "Changes-Requested"
            | "Author-Not-Authorized"
            | "Author-Not-Verified"
            | "Binary-Validation-Error"
            | "Blocking-Issue"
            | "PullRequest-Error"
            | "Validation-Error"
            | "Installer-Error"
            | "URL-Validation-Error"
            | "Hardware"
    ) || label.starts_with("Internal-Error")
        || (label.starts_with("Validation-")
            && (label.ends_with("-Error")
                || label.ends_with("-Failed")
                || label.ends_with("-Mismatch")))
        || (label.starts_with("Manifest-") && label.ends_with("-Error"))
}

fn http_get_json(url: &str, token: Option<&str>) -> Result<String, String> {
    let client = blocking_client(REQUEST_TIMEOUT).map_err(|e| e.to_string())?;
    let mut req = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(tok) = token
        && !tok.is_empty()
    {
        req = req.header("Authorization", format!("Bearer {}", tok));
    }
    let resp = req.send().map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = body_of_blocking(resp);
    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status, body));
    }
    Ok(body)
}

/// Minimal percent-encoder mirroring the preflight implementation. Kept
/// local rather than re-exported from `preflight` so the post-publish
/// module stays independently testable.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for ch in s.chars() {
        match ch {
            ' ' => out.push('+'),
            c if c.is_ascii_alphanumeric() || "-._~/:".contains(c) => out.push(c),
            c => {
                for byte in c.to_string().as_bytes() {
                    out.push('%');
                    out.push_str(&format!("{:02X}", byte));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merged_closed_pr_classifies_as_approved() {
        let v = json!({
            "state": "closed",
            "merged": true,
            "labels": [{"name": "Moderator-Approved"}]
        });
        match classify_pr_json(&v) {
            PrVerdict::Approved(detail) => assert!(detail.contains("merged"), "got: {detail}"),
            other => panic!("expected Approved, got {other:?}"),
        }
    }

    #[test]
    fn closed_unmerged_classifies_as_rejected() {
        let v = json!({
            "state": "closed",
            "merged": false,
            "labels": [{"name": "Validation-Installation-Error"}]
        });
        match classify_pr_json(&v) {
            PrVerdict::Rejected(detail) => {
                assert!(detail.contains("closed without merge"), "got: {detail}");
                assert!(
                    detail.contains("Validation-Installation-Error"),
                    "got: {detail}"
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn open_pr_with_validation_error_classifies_as_rejected() {
        let v = json!({
            "state": "open",
            "merged": false,
            "labels": [
                {"name": "New-Manifest"},
                {"name": "Validation-Hash-Verification-Failed"}
            ]
        });
        match classify_pr_json(&v) {
            PrVerdict::Rejected(detail) => {
                assert_eq!(detail, "Validation-Hash-Verification-Failed")
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn open_pr_with_validation_completed_and_pipeline_passed_classifies_as_approved() {
        let v = json!({
            "state": "open",
            "merged": false,
            "labels": [
                {"name": "Validation-Completed"},
                {"name": "Azure-Pipeline-Passed"}
            ]
        });
        match classify_pr_json(&v) {
            PrVerdict::Approved(detail) => {
                assert!(detail.contains("Validation-Completed"), "got: {detail}")
            }
            other => panic!("expected Approved, got {other:?}"),
        }
    }

    #[test]
    fn open_pr_with_moderator_approved_classifies_as_approved() {
        let v = json!({
            "state": "open",
            "merged": false,
            "labels": [{"name": "Moderator-Approved"}]
        });
        match classify_pr_json(&v) {
            PrVerdict::Approved(detail) => assert_eq!(detail, "Moderator-Approved"),
            other => panic!("expected Approved, got {other:?}"),
        }
    }

    #[test]
    fn open_pr_with_only_in_flight_labels_classifies_as_pending() {
        let v = json!({
            "state": "open",
            "merged": false,
            "labels": [
                {"name": "New-Manifest"},
                {"name": "Needs-Attention"}
            ]
        });
        match classify_pr_json(&v) {
            PrVerdict::Pending(detail) => assert!(detail.contains("New-Manifest"), "got: {detail}"),
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[test]
    fn open_pr_with_no_labels_classifies_as_pending() {
        let v = json!({
            "state": "open",
            "merged": false,
            "labels": []
        });
        match classify_pr_json(&v) {
            PrVerdict::Pending(detail) => assert!(detail.contains("no labels"), "got: {detail}"),
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[test]
    fn needs_cla_classifies_as_rejected() {
        // Verified against live PR #374285 (2026-05-13):
        // closed, unmerged, single label `Needs-CLA` — a real rejection
        // pattern in the wild.
        let v = json!({
            "state": "closed",
            "merged": false,
            "labels": [{"name": "Needs-CLA"}]
        });
        match classify_pr_json(&v) {
            PrVerdict::Rejected(detail) => assert!(detail.contains("Needs-CLA"), "got: {detail}"),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn rejection_label_classification_table() {
        // Pin the rejection-label vocabulary so future label additions
        // get a deliberate decision rather than silent acceptance.
        for label in [
            "Validation-Installation-Error",
            "Validation-Hash-Verification-Failed",
            "Validation-Domains-Mismatch",
            "Manifest-Validation-Error",
            "Internal-Error",
            "Internal-Error-Manifest",
            "PullRequest-Error",
            "Needs-CLA",
            "Changes-Requested",
            "Author-Not-Authorized",
            "Binary-Validation-Error",
            "Blocking-Issue",
            "Hardware",
        ] {
            assert!(
                is_rejection_label(label),
                "{label} should be classified as a rejection label"
            );
        }
        // Counter-set: pending / approval labels must NOT be rejection.
        for label in [
            "New-Manifest",
            "In-PR",
            "Needs-Attention",
            "Validation-Retry",
            "Validation-Completed",
            "Azure-Pipeline-Passed",
            "Moderator-Approved",
            "Manual-Validation-Completed",
            "Manifest-Latest-Update",
        ] {
            assert!(
                !is_rejection_label(label),
                "{label} should NOT be classified as a rejection label"
            );
        }
    }

    #[test]
    fn percent_encoder_matches_preflight() {
        // Matches the encoder in `preflight.rs::percent_encode`. Pin it
        // here so a future divergence is caught at the test boundary.
        assert_eq!(
            percent_encode("repo:microsoft/winget-pkgs is:pr TJSmith.Anodizer 0.2.0 in:title"),
            "repo:microsoft/winget-pkgs+is:pr+TJSmith.Anodizer+0.2.0+in:title"
        );
    }
}
