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
            retry_octocrab_call(policy, "list assets", retry_after, || {
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
                retry_octocrab_call(policy, "delete asset", retry_after, || {
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

/// Look up an existing release asset by name and return its byte size.
///
/// Used by the idempotent-upload path: when GitHub rejects an upload with
/// `422 already_exists`, comparing the existing asset's size to the local
/// file size lets us decide whether a prior attempt successfully uploaded
/// the same bytes (outer-retry recovery) or whether the names collided with
/// different content (real conflict that needs `replace_existing_artifacts`).
///
/// Wrapped in [`retry_octocrab_call`] for the same reason as
/// `delete_release_asset_by_name`: this runs inside the upload retry loop,
/// so a transient 5xx here must not abort the outer recovery path.
pub(crate) async fn find_release_asset_size(
    octo: &Arc<octocrab::Octocrab>,
    owner: &str,
    repo: &str,
    release_id: u64,
    asset_name: &str,
    policy: &RetryPolicy,
    retry_after: Option<&RetryAfterCapture>,
) -> Result<Option<u64>> {
    const MAX_PAGES: u32 = 50;
    let mut page: u32 = 1;
    loop {
        let route = format!(
            "/repos/{}/{}/releases/{}/assets?per_page=100&page={}",
            owner, repo, release_id, page
        );
        let assets: Vec<octocrab::models::repos::Asset> =
            retry_octocrab_call(policy, "list assets", retry_after, || {
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
