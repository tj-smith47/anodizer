use anyhow::{Context as _, Result};

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
/// Returns `Ok(true)` if the asset was found and deleted, `Ok(false)` if not found.
pub(crate) async fn delete_release_asset_by_name(
    octo: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    release_id: u64,
    asset_name: &str,
) -> Result<bool> {
    const MAX_PAGES: u32 = 50; // 50 pages * 100 per page = 5000 assets max
    let mut page: u32 = 1;
    loop {
        let route = format!(
            "/repos/{}/{}/releases/{}/assets?per_page=100&page={}",
            owner, repo, release_id, page
        );
        let assets: Vec<octocrab::models::repos::Asset> =
            octo.get(route, None::<&()>).await.with_context(|| {
                format!(
                    "release: list assets for release {} on {}/{} (page {})",
                    release_id, owner, repo, page
                )
            })?;

        for asset in &assets {
            if asset.name == asset_name {
                octo.repos(owner, repo)
                    .release_assets()
                    .delete(asset.id.into_inner())
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
pub(crate) async fn find_release_asset_size(
    octo: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    release_id: u64,
    asset_name: &str,
) -> Result<Option<u64>> {
    const MAX_PAGES: u32 = 50;
    let mut page: u32 = 1;
    loop {
        let route = format!(
            "/repos/{}/{}/releases/{}/assets?per_page=100&page={}",
            owner, repo, release_id, page
        );
        let assets: Vec<octocrab::models::repos::Asset> =
            octo.get(route, None::<&()>).await.with_context(|| {
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
