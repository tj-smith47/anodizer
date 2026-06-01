//! GitHub Releases API lookups: paginated draft search, tag lookup,
//! published-asset enumeration, post-create readiness probing, and the
//! retention-sweep release listing.
//!
//! These wrap the octocrab client + retry envelope so every read path
//! against `GET /repos/{owner}/{repo}/releases*` shares one source of truth
//! for pagination, 404 handling, and transient-error retry.

use std::sync::Arc;

use anodizer_core::config::{CrateConfig, ReleaseConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::jitter_duration;
use anyhow::{Context as _, Result};

use super::secondary_rate_limit::RetryAfterCapture;
use super::{build_octocrab_client, is_octocrab_404, retry_octocrab_call};
use crate::resolve_release_repo;

/// Page size used when paginating `GET /repos/{owner}/{repo}/releases`.
///
/// Matches GitHub's per-page maximum so the draft search reaches the
/// answer in the minimum number of round trips. The "fewer than this many
/// results means last page" pagination terminator depends on this value
/// being the same as the `per_page` query parameter sent in
/// [`find_draft_by_name`].
const LIST_RELEASES_PAGE_SIZE: usize = 100;

/// Find a draft release on `{owner}/{repo}` whose `name` field matches
/// `name`, paginating through `GET /repos/{owner}/{repo}/releases` 100
/// results at a time until a match is found or the listing is exhausted.
///
/// GoReleaser parity: mirrors `internal/client/github.go::findDraftRelease`,
/// which searches releases by *name* (not tag) and loops while
/// `resp.NextPage != 0`. There is no artificial page cap so repos with
/// thousands of historical draft releases still locate the target —
/// otherwise the create-release path would 422 on a duplicate tag.
///
/// Each page fetch is wrapped by [`retry_octocrab_call`] so transient
/// 5xx / 429 / transport failures retry according to `policy`; 4xx
/// errors (auth, validation) fast-fail. The retry envelope wraps a single
/// page only: once a page returns OK, the next page is fetched fresh.
///
/// Returns `Ok(Some(release))` when a draft with the matching `name` is
/// found, `Ok(None)` when the listing is exhausted with no match, and
/// `Err(_)` when a non-retryable error surfaces (or every retry has been
/// consumed).
pub(crate) async fn find_draft_by_name(
    octo: &Arc<octocrab::Octocrab>,
    owner: &str,
    repo: &str,
    name: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    retry_after: Option<&RetryAfterCapture>,
) -> Result<Option<octocrab::models::repos::Release>> {
    let mut page: u32 = 1;
    loop {
        let route = format!(
            "/repos/{}/{}/releases?per_page={}&page={}",
            owner, repo, LIST_RELEASES_PAGE_SIZE, page
        );
        let releases: Vec<octocrab::models::repos::Release> =
            retry_octocrab_call(policy, "list releases", retry_after, || {
                let route = route.clone();
                let octo = octo.clone();
                async move { octo.get(route, None::<&()>).await }
            })
            .await
            .with_context(|| {
                format!(
                    "release: list releases on {}/{} (page {})",
                    owner, repo, page
                )
            })?;
        if let Some(found) = releases
            .iter()
            .find(|r| r.draft && r.name.as_deref() == Some(name))
        {
            return Ok(Some(found.clone()));
        }
        if releases.len() < LIST_RELEASES_PAGE_SIZE {
            break;
        }
        page += 1;
    }
    Ok(None)
}

/// Look up the single release that points at `tag` via the GitHub Releases API.
///
/// Returns `Ok(Some(release))` when a release exists for the tag,
/// `Ok(None)` when the tag has no associated release (HTTP 404), and
/// `Err(_)` when any other error surfaces (auth, validation, exhausted retries
/// on 5xx / 429) so the caller sees the real GitHub error rather than silently
/// treating a failed lookup as "no existing release".
pub(super) async fn find_release_by_tag(
    octo: &Arc<octocrab::Octocrab>,
    owner: &str,
    repo: &str,
    tag: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    retry_after: Option<&RetryAfterCapture>,
    label: &'static str,
) -> Result<Option<octocrab::models::repos::Release>> {
    let owner = owner.to_string();
    let repo = repo.to_string();
    let tag = tag.to_string();
    let result: Result<octocrab::models::repos::Release, octocrab::Error> =
        retry_octocrab_call(policy, label, retry_after, || {
            let octo = octo.clone();
            let owner = owner.clone();
            let repo = repo.clone();
            let tag = tag.clone();
            async move { octo.repos(&owner, &repo).releases().get_by_tag(&tag).await }
        })
        .await;
    match result {
        Ok(release) => Ok(Some(release)),
        Err(err) if is_octocrab_404(&err) => Ok(None),
        Err(err) => Err(anyhow::Error::new(err)),
    }
}

/// Fetch the names of the assets currently UPLOADED to the published
/// GitHub release for `crate_cfg`'s resolved tag.
///
/// This is the network half of the post-release asset-existence check: the
/// verify-release stage diffs this live, GitHub-stored set against the
/// produced artifact set to catch the partial uploads GitHub silently
/// tolerates. It reuses the hardened release backend's repo-resolution
/// ([`resolve_release_repo`]), tag-resolution
/// ([`resolve_release_tag`](crate::release_body::resolve_release_tag)), and
/// octocrab client/retry path so there is one source of truth for "how do we
/// talk to the GitHub Releases API".
///
/// Returns:
/// - `Ok(Some(names))` — the release exists; `names` are its asset names
///   (empty vec when the release has no assets).
/// - `Ok(None)` — no GitHub repo is configured for the active token type
///   (the verify stage treats this as "not a GitHub release; skip the asset
///   check for this crate" rather than an error).
///
/// Errors when the tag has no release (the publish should have created it —
/// a genuine post-publish defect), when no token is available, or when the
/// GitHub API call fails after retries.
pub async fn fetch_published_asset_names(
    ctx: &Context,
    release_cfg: &ReleaseConfig,
    crate_cfg: &CrateConfig,
) -> Result<Option<Vec<String>>> {
    let Some(repo) = resolve_release_repo(release_cfg, ctx.token_type, ctx)? else {
        return Ok(None);
    };

    let tag = crate::release_body::resolve_release_tag(
        ctx,
        &crate_cfg.tag_template,
        release_cfg.tag.as_deref(),
        &crate_cfg.name,
    )?;

    let token = ctx.options.token.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "verify-release: no GitHub token available to fetch the published \
             release's assets (set GITHUB_TOKEN or ANODIZER_GITHUB_TOKEN, or pass --token)"
        )
    })?;

    let github_urls = ctx.config.github_urls.clone();
    let policy = ctx.retry_policy();

    let (octo_raw, retry_after) = build_octocrab_client(&token, &github_urls)?;
    let octo = Arc::new(octo_raw);

    let release = find_release_by_tag(
        &octo,
        &repo.owner,
        &repo.name,
        &tag,
        &policy,
        Some(&retry_after),
        "verify-release fetch published assets",
    )
    .await?;

    match release {
        Some(rel) => Ok(Some(rel.assets.into_iter().map(|a| a.name).collect())),
        None => anyhow::bail!(
            "verify-release: no GitHub release found for tag '{}' on {}/{} — \
             the publish should have created it; this is a post-publish defect",
            tag,
            repo.owner,
            repo.name
        ),
    }
}

/// Number of `GET /releases/{id}` readiness probes attempted before the
/// upload loop starts (see [`wait_for_release_readable`]). The 7 inter-probe
/// sleeps double from [`READINESS_GUARD_BASE_DELAY`] (100 ms) and saturate at
/// [`READINESS_GUARD_MAX_DELAY`] (1500 ms) — 100+200+400+800+1500+1500+1500 ≈
/// 6 s nominal, ~7 s with jitter, leaving headroom under the ~10 s budget so
/// the guard never dominates release wall-clock.
const READINESS_GUARD_ATTEMPTS: u32 = 8;

/// Initial backoff between readiness probes; doubles each slot up to
/// [`READINESS_GUARD_MAX_DELAY`].
const READINESS_GUARD_BASE_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// Per-slot ceiling for the readiness-probe backoff.
const READINESS_GUARD_MAX_DELAY: std::time::Duration = std::time::Duration::from_millis(1500);

/// Poll `GET /repos/{owner}/{repo}/releases/{id}` until it returns 200,
/// bounded by [`READINESS_GUARD_ATTEMPTS`] with short exponential backoff.
///
/// GitHub serves `POST /releases` from a primary replica but the
/// `GET /releases/{id}` issued by `ReleasesHandler::upload_asset(...).send()`
/// (to read the release's `upload_url`) may hit a replica that has not yet
/// observed the create — a read-after-write lag that surfaces as a transient
/// 404. Because the upload loop fans out in parallel immediately after the
/// create, several of those probes can race the propagation window at once.
///
/// This guard makes the release readable once before any upload starts,
/// shrinking (but not eliminating — replicas lag independently) that window.
/// It runs regardless of the resolved retry policy's `max_attempts`, because
/// it is a consistency guard rather than a flaky-network retry. On persistent
/// failure after the bound it returns `Ok(false)` so the caller proceeds
/// anyway: the per-upload bounded-404 retry is the backstop, and this guard
/// must never introduce a new failure mode of its own.
///
/// Returns `Ok(true)` once the release is readable (immediately on the first
/// probe in the common no-lag case), `Ok(false)` if the bound is exhausted
/// without a 200, and `Err(_)` only for a non-404 hard error (auth, etc.).
pub(super) async fn wait_for_release_readable(
    octo: &Arc<octocrab::Octocrab>,
    owner: &str,
    repo: &str,
    release_id: u64,
    log: &StageLogger,
) -> Result<bool> {
    let mut delay = READINESS_GUARD_BASE_DELAY;
    for attempt in 1..=READINESS_GUARD_ATTEMPTS {
        let route = format!("/repos/{owner}/{repo}/releases/{release_id}");
        let result = octo
            .get::<octocrab::models::repos::Release, _, _>(route, None::<&()>)
            .await;
        match result {
            Ok(_) => {
                if attempt > 1 {
                    log.verbose(&format!(
                        "release {release_id} became readable after {attempt} probe(s) \
                         (GitHub post-create propagation lag)"
                    ));
                }
                return Ok(true);
            }
            Err(err) if is_octocrab_404(&err) => {
                if attempt < READINESS_GUARD_ATTEMPTS {
                    tokio::time::sleep(jitter_duration(delay)).await;
                    delay = std::cmp::min(delay * 2, READINESS_GUARD_MAX_DELAY);
                }
            }
            // A non-404 hard error (auth, validation) is not a propagation
            // lag; surface it rather than silently consuming the budget.
            Err(err) => return Err(anyhow::Error::new(err)),
        }
    }
    Ok(false)
}

/// List all releases on `{owner}/{repo}` whose `name` field equals `name`,
/// returning `(id, tag_name)` pairs in the order GitHub returns them
/// (newest-first — the Releases API lists by `created_at` descending).
///
/// Used by the nightly retention sweep to enumerate prior nightly releases
/// sharing the rendered nightly release name (the per-build differentiator
/// lives in the TAG, not the name, so the name is the stable matcher).
pub(super) async fn list_releases_by_name(
    octo: &Arc<octocrab::Octocrab>,
    owner: &str,
    repo: &str,
    name: &str,
    policy: &anodizer_core::retry::RetryPolicy,
    retry_after: Option<&RetryAfterCapture>,
) -> Result<Vec<(u64, String)>> {
    let mut out = Vec::new();
    let mut page: u32 = 1;
    loop {
        let route = format!(
            "/repos/{}/{}/releases?per_page={}&page={}",
            owner, repo, LIST_RELEASES_PAGE_SIZE, page
        );
        let releases: Vec<octocrab::models::repos::Release> =
            retry_octocrab_call(policy, "list releases (retention)", retry_after, || {
                let route = route.clone();
                let octo = octo.clone();
                async move { octo.get(route, None::<&()>).await }
            })
            .await
            .with_context(|| {
                format!(
                    "release: list releases on {}/{} for retention (page {})",
                    owner, repo, page
                )
            })?;
        let page_len = releases.len();
        for r in releases {
            if r.name.as_deref() == Some(name) {
                out.push((r.id.into_inner(), r.tag_name));
            }
        }
        if page_len < LIST_RELEASES_PAGE_SIZE {
            break;
        }
        page += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod find_draft_by_name_tests {
    //! Behavioural pins for [`find_draft_by_name`] — the paginated draft
    //! search used by the `replace_existing_draft` and
    //! `use_existing_draft` paths in `run_github_backend`.
    //!
    //! These tests drive a real `octocrab::Octocrab` against an
    //! in-process loopback responder (the shared
    //! `spawn_oneshot_http_responder`) so the pagination terminator,
    //! per-page route shape, and `draft && name match` predicate are
    //! pinned against the production code path — not the matcher in
    //! isolation.
    use super::*;
    use crate::test_support::{build_test_octocrab, test_retry_policy};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use std::sync::atomic::Ordering;

    /// Build a minimal release JSON list of `count` entries, marking the
    /// one at `match_idx` (when `Some`) as a draft with `name=target`.
    /// Every other entry is published (`draft: false`) with a distinct
    /// name so the predicate's "draft && name match" requirement is
    /// exercised.
    fn build_release_list_body(
        count: usize,
        match_idx: Option<usize>,
        target_name: &str,
    ) -> String {
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let (draft, name) = match match_idx {
                Some(idx) if idx == i => (true, target_name.to_string()),
                _ => (false, format!("other-release-{i}")),
            };
            entries.push(serde_json::json!({
                "id": 1000 + i as u64,
                "node_id": format!("RL_{i}"),
                "tag_name": format!("v0.0.{i}"),
                "target_commitish": "main",
                "name": name,
                "draft": draft,
                "prerelease": false,
                "created_at": "2026-01-01T00:00:00Z",
                "published_at": null,
                "author": null,
                "assets": [],
                "tarball_url": null,
                "zipball_url": null,
                "body": null,
                "url": format!("https://api.github.com/repos/o/r/releases/{}", 1000 + i),
                "html_url": format!("https://github.com/o/r/releases/{}", 1000 + i),
                "assets_url": format!("https://api.github.com/repos/o/r/releases/{}/assets", 1000 + i),
                "upload_url": format!("https://uploads.github.com/repos/o/r/releases/{}/assets{{?name,label}}", 1000 + i),
            }));
        }
        serde_json::Value::Array(entries).to_string()
    }

    /// Build a static HTTP response carrying a JSON release-list body.
    fn build_release_list_response(body: String) -> &'static str {
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body,
        );
        Box::leak(raw.into_boxed_str())
    }

    #[tokio::test]
    async fn single_page_with_matching_draft_returns_some() {
        let body = build_release_list_body(3, Some(1), "v1.2.3");
        let (addr, calls) = spawn_oneshot_http_responder(vec![build_release_list_response(body)]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();
        let found = find_draft_by_name(&octo, "o", "r", "v1.2.3", &policy, None)
            .await
            .expect("draft search must succeed");
        let release = found.expect("draft with matching name must be found");
        assert_eq!(release.name.as_deref(), Some("v1.2.3"));
        assert!(release.draft, "matched release must be a draft");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "single-page search must issue exactly one list-releases call",
        );
    }

    #[tokio::test]
    async fn single_page_no_match_returns_none() {
        // Three published releases, none match the target name; the
        // predicate must not coerce a non-draft into a match.
        let body = build_release_list_body(3, None, "v9.9.9");
        let (addr, _calls) = spawn_oneshot_http_responder(vec![build_release_list_response(body)]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();
        let found = find_draft_by_name(&octo, "o", "r", "v9.9.9", &policy, None)
            .await
            .expect("draft search must succeed");
        assert!(
            found.is_none(),
            "no draft matches the target name => Ok(None)",
        );
    }

    #[tokio::test]
    async fn name_matches_but_not_draft_returns_none() {
        // A *published* release whose name equals the target must NOT
        // match — the predicate requires `draft && name match`.
        let body = build_release_list_body(2, None, "ignored");
        // Patch entry 0 to have the target name but stay non-draft.
        let mut entries: Vec<serde_json::Value> = serde_json::from_str(&body).expect("array");
        entries[0]["name"] = serde_json::Value::String("v1.2.3".to_string());
        entries[0]["draft"] = serde_json::Value::Bool(false);
        let body = serde_json::Value::Array(entries).to_string();
        let (addr, _calls) = spawn_oneshot_http_responder(vec![build_release_list_response(body)]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();
        let found = find_draft_by_name(&octo, "o", "r", "v1.2.3", &policy, None)
            .await
            .expect("draft search must succeed");
        assert!(
            found.is_none(),
            "published release with matching name must NOT count as a draft hit",
        );
    }

    #[tokio::test]
    async fn paginates_across_pages_until_match_found() {
        // Page 1: 100 non-matching published releases (forces another page).
        // Page 2: a draft with the matching name in slot 0.
        let page_1 = build_release_list_body(100, None, "v1.2.3");
        let page_2 = build_release_list_body(5, Some(0), "v1.2.3");
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            build_release_list_response(page_1),
            build_release_list_response(page_2),
        ]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();
        let found = find_draft_by_name(&octo, "o", "r", "v1.2.3", &policy, None)
            .await
            .expect("paginated draft search must succeed");
        let release = found.expect("draft on page 2 must be found");
        assert_eq!(release.name.as_deref(), Some("v1.2.3"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "pagination must consume exactly 2 list-releases calls (full first page + second page)",
        );
    }

    #[tokio::test]
    async fn paginates_to_exhaustion_returns_none() {
        // Page 1: 100 non-matching entries (full page => continue).
        // Page 2: 50 non-matching entries (< page size => terminate).
        let page_1 = build_release_list_body(100, None, "missing");
        let page_2 = build_release_list_body(50, None, "missing");
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            build_release_list_response(page_1),
            build_release_list_response(page_2),
        ]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();
        let found = find_draft_by_name(&octo, "o", "r", "missing", &policy, None)
            .await
            .expect("draft search must succeed even when no match");
        assert!(
            found.is_none(),
            "exhausted listing with no match => Ok(None)",
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "must fetch both pages before terminating on the partial page",
        );
    }
}

#[cfg(test)]
mod get_by_tag_lookup_tests {
    //! Pin the `get_by_tag` lookup decision rule introduced to prevent the
    //! "transient 5xx falls through to create-release POST" bug.
    //!
    //! Two invariants:
    //! 1. The lookup is retried per the user's `RetryPolicy` (transient 5xx /
    //!    429 / transport failures retry). The retry-loop contract itself is
    //!    pinned by `retry_call::tests` against a real TCP responder.
    //! 2. Only a real 404 yields "no existing release" (None); every other
    //!    error (auth, validation, exhausted retries on 5xx) propagates so
    //!    the user sees the real GitHub error, NOT a downstream 422
    //!    "tag already exists" from the create-release POST.
    //!
    //! The tests below focus on the routing predicate `is_octocrab_404`
    //! against real `octocrab::Error::GitHub` values. The retry-then-error
    //! coupling is exercised by `retry_call::tests` plus a single 404
    //! fast-fail check here so the predicate's "404 only" invariant is
    //! pinned end-to-end against the helper.
    use super::*;
    use anodizer_core::retry::RetryPolicy;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use std::net::SocketAddr;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    #[tokio::test]
    async fn is_octocrab_404_matches_only_404_github_variant() {
        // The pure predicate's contract: returns true for
        // `Error::GitHub { source }` with status_code 404, false for every
        // other variant or status.
        let github_err_404 = synth_github_error(404).await;
        assert!(
            is_octocrab_404(&github_err_404),
            "404 status_code on GitHub variant must classify as 404"
        );
        let github_err_503 = synth_github_error(503).await;
        assert!(
            !is_octocrab_404(&github_err_503),
            "503 must NOT classify as 404 (would let the caller fall \
             through to create-release and surface a downstream 422)"
        );
        let github_err_422 = synth_github_error(422).await;
        assert!(
            !is_octocrab_404(&github_err_422),
            "422 must NOT classify as 404"
        );
        let github_err_500 = synth_github_error(500).await;
        assert!(
            !is_octocrab_404(&github_err_500),
            "500 must NOT classify as 404"
        );
    }

    #[tokio::test]
    async fn get_by_tag_404_fast_fails_through_helper_to_predicate() {
        // End-to-end: drive a 404 through `retry_octocrab_call` and confirm
        // the returned typed error satisfies `is_octocrab_404`, so the
        // backend's match arm maps the lookup to "no existing release"
        // (the only non-error fall-through to create-release).
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 23\r\n\r\n{\"message\":\"Not Found\"}",
        ]);
        let octo = build_test_octocrab(addr);
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result: Result<Vec<serde_json::Value>, octocrab::Error> =
            retry_octocrab_call(&policy, "get release by tag", None, || async {
                octo.get("/repos/owner/repo/releases/tags/v1.0.0", None::<&()>)
                    .await
            })
            .await;
        assert!(result.is_err(), "404 must surface as Err from the helper");
        let err = result.expect_err("err is Some by the assert above");
        assert!(
            is_octocrab_404(&err),
            "404 must classify so the caller maps to None: got {err:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "404 must NOT retry (fast-fail honors classifier)"
        );
    }

    #[tokio::test]
    async fn get_by_tag_5xx_retries_then_succeeds_under_helper() {
        // Pin the regression: a transient 5xx on `get_by_tag` must retry
        // through `retry_octocrab_call`, NOT fall through to the
        // create-release POST (which would surface a 422 "tag already
        // exists" on a tag whose existing release just had a flaky lookup).
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        ]);
        let octo = build_test_octocrab(addr);
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result: Result<serde_json::Value, octocrab::Error> =
            retry_octocrab_call(&policy, "get release by tag", None, || async {
                octo.get("/repos/owner/repo/releases/tags/v1.0.0", None::<&()>)
                    .await
            })
            .await;
        assert!(
            result.is_ok(),
            "5xx must retry to success under the get_by_tag label: {:?}",
            result.err()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "expected 2 retries past 5xx + 1 success"
        );
    }

    #[tokio::test]
    async fn get_by_tag_500_forever_surfaces_real_error_not_404_fallthrough() {
        // Pin the regression: if every retry sees 5xx, the helper must
        // surface the typed 500 error (NOT swallow it into None). The
        // backend's match arm has only one non-error fall-through (a real
        // 404 via `is_octocrab_404`); 500-forever must propagate so the
        // user sees the real GitHub error instead of a confusing downstream
        // 422 "tag already exists" from create-release.
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        ]);
        let octo = build_test_octocrab(addr);
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let result: Result<serde_json::Value, octocrab::Error> =
            retry_octocrab_call(&policy, "get release by tag", None, || async {
                octo.get("/repos/owner/repo/releases/tags/v1.0.0", None::<&()>)
                    .await
            })
            .await;
        assert!(
            result.is_err(),
            "500-forever must surface as Err, NOT swallow into None"
        );
        let err = result.expect_err("err is Some by the assert above");
        assert!(
            !is_octocrab_404(&err),
            "500-forever must NOT classify as 404; the backend's only \
             non-error fall-through is 404, so misclassifying here would \
             trigger the original bug: get_by_tag 5xx -> create-release \
             POST -> 422. Got: {err:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "max_attempts=3 must produce exactly 3 octocrab calls"
        );
    }

    /// Synthesize an `octocrab::Error::GitHub` with a chosen status code by
    /// round-tripping a minimal GitHub error body through the live API
    /// envelope. octocrab's `*Snafu` builders are private, so we cannot
    /// construct the variant directly; the canonical path is to drive an
    /// HTTP response through octocrab and capture the resulting `Err`.
    async fn synth_github_error(status: u16) -> octocrab::Error {
        let body = serde_json::json!({
            "message": "synthetic",
            "documentation_url": "https://example/synthetic"
        })
        .to_string();
        let resp = format!(
            "HTTP/1.1 {status} STATUS\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body,
        );
        let static_resp: &'static str = Box::leak(resp.into_boxed_str());
        let (addr, _calls) = spawn_oneshot_http_responder(vec![static_resp]);
        let octo = build_test_octocrab(addr);
        octo.get::<serde_json::Value, _, _>("/synthetic", None::<&()>)
            .await
            .expect_err("synth_github_error: octocrab must surface Err for non-2xx status")
    }

    fn build_test_octocrab(addr: SocketAddr) -> octocrab::Octocrab {
        let builder = octocrab::OctocrabBuilder::new()
            .base_uri(format!("http://{addr}/"))
            .expect("OctocrabBuilder::base_uri accepts loopback URL");
        builder
            .build()
            .expect("OctocrabBuilder::build succeeds on loopback URL")
    }
}
