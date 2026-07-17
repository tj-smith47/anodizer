use anyhow::{Context as _, Result};
use std::sync::Arc;

use super::{RetryAfterCapture, retry_octocrab_call};
use anodizer_core::retry::RetryPolicy;

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
/// Every API call flows through [`retry_octocrab_call`] so transient
/// 5xx/429/secondary-rate-limit responses retry per the resolved
/// [`RetryPolicy`]. This runs inside the upload retry loop's
/// `already_exists` recovery, so a transient 5xx here must not abort
/// the outer recovery path.
///
/// Returns `Ok(true)` if the asset was found and deleted, `Ok(false)` if not found.
pub(crate) async fn delete_release_asset_by_name(
    octo: &Arc<octocrab::Octocrab>,
    owner: &str,
    repo: &str,
    release_id: u64,
    asset_name: &str,
    policy: &RetryPolicy,
    retry_after: Option<&RetryAfterCapture>,
) -> Result<bool> {
    const MAX_PAGES: u32 = 50; // 50 pages * 100 per page = 5000 assets max
    let mut page: u32 = 1;
    loop {
        let route = format!(
            "/repos/{}/{}/releases/{}/assets?per_page=100&page={}",
            owner, repo, release_id, page
        );
        let assets: Vec<octocrab::models::repos::Asset> =
            retry_octocrab_call(policy, None, "list assets", retry_after, || {
                let route = route.clone();
                let octo = octo.clone();
                async move { octo.get(route, None::<&()>).await }
            })
            .await
            .with_context(|| {
                format!(
                    "release: list assets for release {} on {}/{} (page {})",
                    release_id, owner, repo, page
                )
            })?;

        for asset in &assets {
            if asset.name == asset_name {
                let asset_id = asset.id.into_inner();
                let owner_s = owner.to_string();
                let repo_s = repo.to_string();
                retry_octocrab_call(policy, None, "delete asset", retry_after, || {
                    let octo = octo.clone();
                    let owner_s = owner_s.clone();
                    let repo_s = repo_s.clone();
                    async move {
                        octo.repos(owner_s, repo_s)
                            .release_assets()
                            .delete(asset_id)
                            .await
                    }
                })
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

/// What the remote already holds for an asset name, as probed after a
/// `422 already_exists` rejection.
///
/// `uploaded` is GitHub's asset `state == "uploaded"`. An interrupted
/// upload (network drop, transient 401/5xx mid-transfer) can leave the
/// asset registered in a non-`uploaded` state (`"starter"`) — a partial
/// that blocks same-name re-uploads with `already_exists` while never
/// being downloadable. The upload retry loop treats `uploaded: false`
/// as "delete and retry" regardless of `replace_existing_artifacts`,
/// because a partial is this run's own debris, not published content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteAssetProbe {
    pub(crate) size: u64,
    pub(crate) uploaded: bool,
    /// GitHub's `sha256:<hex>` content digest, when the API serves one.
    /// `None` on GHES versions / asset ages that predate digest exposure —
    /// callers fall back to size-only comparison in that case.
    pub(crate) digest: Option<String>,
}

/// Look up an existing release asset by name and return its byte size +
/// upload state.
///
/// Used by the idempotent-upload path: when GitHub rejects an upload with
/// `422 already_exists`, comparing the existing asset's size to the local
/// file size lets us decide whether a prior attempt successfully uploaded
/// the same bytes (outer-retry recovery) or whether the names collided with
/// different content (real conflict that needs `replace_existing_artifacts`).
/// The `state` field distinguishes a fully-published asset from a partial
/// left behind by an interrupted upload.
///
/// Wrapped in [`retry_octocrab_call`] for the same reason as
/// `delete_release_asset_by_name`: this runs inside the upload retry loop,
/// so a transient 5xx here must not abort the outer recovery path.
pub(crate) async fn find_release_asset_probe(
    octo: &Arc<octocrab::Octocrab>,
    owner: &str,
    repo: &str,
    release_id: u64,
    asset_name: &str,
    policy: &RetryPolicy,
    retry_after: Option<&RetryAfterCapture>,
) -> Result<Option<RemoteAssetProbe>> {
    const MAX_PAGES: u32 = 50;
    let mut page: u32 = 1;
    loop {
        let route = format!(
            "/repos/{}/{}/releases/{}/assets?per_page=100&page={}",
            owner, repo, release_id, page
        );
        let assets: Vec<octocrab::models::repos::Asset> =
            retry_octocrab_call(policy, None, "list assets", retry_after, || {
                let route = route.clone();
                let octo = octo.clone();
                async move { octo.get(route, None::<&()>).await }
            })
            .await
            .with_context(|| {
                format!(
                    "release: list assets for release {} on {}/{} (page {})",
                    release_id, owner, repo, page
                )
            })?;

        for asset in &assets {
            if asset.name == asset_name {
                return Ok(Some(RemoteAssetProbe {
                    size: asset.size as u64,
                    uploaded: asset.state == "uploaded",
                    digest: asset.digest.clone(),
                }));
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

#[cfg(test)]
mod tests {
    //! End-to-end coverage for the asset list/delete helpers via the shared
    //! in-process HTTP responder. Mirrors `retry_call.rs`'s test convention:
    //! point `Octocrab` at a loopback responder, script canned HTTP responses,
    //! and assert both the returned value AND the request counter so a
    //! regression in the pagination / retry plumbing is caught (not just the
    //! happy-path return value).
    use super::*;
    use crate::test_support::{build_test_octocrab, test_retry_policy};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use std::sync::atomic::Ordering;

    /// JSON for a single Asset matching octocrab's `models::repos::Asset`
    /// shape. The struct requires every field (no `#[serde(default)]`), so
    /// the fixture has to populate all of them — only `name`, `size`, and
    /// `state` are load-bearing for the function under test; the rest are
    /// stub values.
    fn asset_json(name: &str, size: u64, id: u64) -> String {
        asset_json_with_state(name, size, id, "uploaded")
    }

    fn asset_json_with_state(name: &str, size: u64, id: u64, state: &str) -> String {
        asset_json_with_digest(name, size, id, state, None)
    }

    fn asset_json_with_digest(
        name: &str,
        size: u64,
        id: u64,
        state: &str,
        digest: Option<&str>,
    ) -> String {
        let digest_field = match digest {
            Some(d) => format!("\"{d}\""),
            None => "null".to_string(),
        };
        format!(
            r#"{{
                "url": "https://api.github.com/repos/o/r/releases/assets/{id}",
                "browser_download_url": "https://github.com/o/r/releases/download/v1/{name}",
                "id": {id},
                "node_id": "RA_kwDO",
                "name": "{name}",
                "label": null,
                "state": "{state}",
                "content_type": "application/gzip",
                "size": {size},
                "digest": {digest_field},
                "download_count": 0,
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
                "uploader": null
            }}"#
        )
    }

    /// Wrap a JSON body in a `200 OK` HTTP response with a correct
    /// `Content-Length`. The responder helper requires `&'static str`, so
    /// we `Box::leak` the formatted string — fine in tests, no production
    /// cost.
    fn ok_json(body: String) -> &'static str {
        let len = body.len();
        Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}"
            )
            .into_boxed_str(),
        )
    }

    /// Build a JSON array string containing `count` assets all named the
    /// same non-matching name. Used to fill page-1 with exactly 100 entries
    /// so the pagination loop is forced to fetch page 2.
    fn full_page_no_match(count: usize) -> String {
        let entries: Vec<String> = (0..count)
            .map(|i| asset_json("filler.bin", 1, 1000 + i as u64))
            .collect();
        format!("[{}]", entries.join(","))
    }

    const RESP_204: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";
    const RESP_503: &str = "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";

    // ------------------------------------------------------------------
    // find_release_asset_probe
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn find_returns_some_when_asset_matches_first_page() {
        let body = format!("[{}]", asset_json("anodize-v1.tar.gz", 4242, 42));
        let (addr, calls) = spawn_oneshot_http_responder(vec![ok_json(body)]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();

        let got = find_release_asset_probe(&octo, "o", "r", 1, "anodize-v1.tar.gz", &policy, None)
            .await
            .expect("call succeeds");

        assert_eq!(
            got,
            Some(RemoteAssetProbe {
                size: 4242,
                uploaded: true,
                digest: None
            }),
            "must surface the matching asset's size and uploaded state"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "single page with a match must NOT paginate further"
        );
    }

    #[tokio::test]
    async fn find_reports_partial_state_for_interrupted_upload() {
        // An interrupted upload leaves the asset registered with
        // state "starter" (not "uploaded"). The probe must surface
        // `uploaded: false` so the retry loop deletes the partial
        // instead of treating it as published content.
        let body = format!(
            "[{}]",
            asset_json_with_state("broken.tar.gz", 5, 13, "starter")
        );
        let (addr, _calls) = spawn_oneshot_http_responder(vec![ok_json(body)]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();

        let got = find_release_asset_probe(&octo, "o", "r", 1, "broken.tar.gz", &policy, None)
            .await
            .expect("call succeeds");

        assert_eq!(
            got,
            Some(RemoteAssetProbe {
                size: 5,
                uploaded: false,
                digest: None
            }),
            "non-'uploaded' state must probe as uploaded: false"
        );
    }

    #[tokio::test]
    async fn find_returns_none_when_no_asset_matches() {
        let (addr, calls) = spawn_oneshot_http_responder(vec![ok_json("[]".to_string())]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();

        let got = find_release_asset_probe(&octo, "o", "r", 1, "missing.tar.gz", &policy, None)
            .await
            .expect("call succeeds");

        assert_eq!(got, None, "empty list must yield None, not an error");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "empty page (< 100) must terminate pagination immediately"
        );
    }

    #[tokio::test]
    async fn find_surfaces_remote_digest_when_the_api_serves_one() {
        // Digest-aware idempotency (upload.rs's classify_already_exists)
        // needs the remote's `sha256:` digest, not just its size — this
        // pins that the probe actually threads `Asset.digest` through.
        let body = format!(
            "[{}]",
            asset_json_with_digest(
                "app.tar.gz",
                100,
                7,
                "uploaded",
                Some("sha256:deadbeef00112233"),
            )
        );
        let (addr, _calls) = spawn_oneshot_http_responder(vec![ok_json(body)]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();

        let got = find_release_asset_probe(&octo, "o", "r", 1, "app.tar.gz", &policy, None)
            .await
            .expect("call succeeds");

        assert_eq!(
            got,
            Some(RemoteAssetProbe {
                size: 100,
                uploaded: true,
                digest: Some("sha256:deadbeef00112233".to_string()),
            }),
            "the API-served digest must be surfaced on the probe"
        );
    }

    #[tokio::test]
    async fn find_paginates_to_page_two_when_page_one_is_full() {
        // Page 1: 100 non-matching entries -> loop must fetch page 2.
        // Page 2: one match -> Some(probe).
        let page1 = ok_json(full_page_no_match(100));
        let page2 = ok_json(format!("[{}]", asset_json("target.zip", 999, 7)));
        let (addr, calls) = spawn_oneshot_http_responder(vec![page1, page2]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();

        let got = find_release_asset_probe(&octo, "o", "r", 1, "target.zip", &policy, None)
            .await
            .expect("call succeeds");

        assert_eq!(
            got.map(|p| p.size),
            Some(999),
            "must find the match on page 2"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "page-1 full (100 entries) must force a page-2 fetch"
        );
    }

    #[tokio::test]
    async fn find_retries_on_transient_5xx() {
        // 503 then a 200 with a match. The retry helper should swallow the
        // 503 and surface the eventual match.
        let body = format!("[{}]", asset_json("retry-me.tar.gz", 7, 11));
        let (addr, calls) = spawn_oneshot_http_responder(vec![RESP_503, ok_json(body)]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();

        let got = find_release_asset_probe(&octo, "o", "r", 1, "retry-me.tar.gz", &policy, None)
            .await
            .expect("must retry past 503 to success");

        assert_eq!(got.map(|p| p.size), Some(7));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "expected 1 retried 503 + 1 success = 2 HTTP attempts"
        );
    }

    // ------------------------------------------------------------------
    // delete_release_asset_by_name
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn delete_returns_false_when_asset_absent() {
        let (addr, calls) = spawn_oneshot_http_responder(vec![ok_json("[]".to_string())]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();

        let got = delete_release_asset_by_name(&octo, "o", "r", 1, "ghost.bin", &policy, None)
            .await
            .expect("call succeeds");

        assert!(!got, "absent asset must report not-found, not an error");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "absent asset must NOT issue a DELETE request"
        );
    }

    #[tokio::test]
    async fn delete_returns_true_after_successful_delete() {
        // List returns the match, then DELETE responds 204.
        let list_body = format!("[{}]", asset_json("kill.tar.gz", 1, 99));
        let (addr, calls) = spawn_oneshot_http_responder(vec![ok_json(list_body), RESP_204]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();

        let got = delete_release_asset_by_name(&octo, "o", "r", 1, "kill.tar.gz", &policy, None)
            .await
            .expect("call succeeds");

        assert!(got, "successful delete must report true");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "expected exactly 2 HTTP calls: 1 list + 1 delete"
        );
    }

    #[tokio::test]
    async fn delete_retries_on_transient_5xx_in_list_call() {
        // 503 on first list -> retry -> list-with-match -> 204 delete.
        let list_body = format!("[{}]", asset_json("kill.tar.gz", 1, 99));
        let (addr, calls) =
            spawn_oneshot_http_responder(vec![RESP_503, ok_json(list_body), RESP_204]);
        let octo = build_test_octocrab(addr);
        let policy = test_retry_policy();

        let got = delete_release_asset_by_name(&octo, "o", "r", 1, "kill.tar.gz", &policy, None)
            .await
            .expect("must retry past 503 to successful delete");

        assert!(
            got,
            "delete must report true after the retried path succeeds"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "expected 1 retried 503 + 1 list success + 1 delete = 3 HTTP attempts"
        );
    }
}
