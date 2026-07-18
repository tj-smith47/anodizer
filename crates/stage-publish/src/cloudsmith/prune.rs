use super::*;

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
pub(crate) fn prune_cloudsmith_versions(
    client: &reqwest::blocking::Client,
    api_base: &str,
    organization: &str,
    repository: &str,
    package_name: &str,
    current_version: &str,
    keep: u32,
    token: &str,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
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
        deadline,
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
        match retry_request(
            "packages/prune-delete",
            package_name,
            policy,
            deadline,
            log,
            || {
                client
                    .delete(&url)
                    .header("Authorization", format!("token {}", token))
                    .header("Accept", "application/json")
                    .send()
            },
        ) {
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
pub(crate) fn retained_version_summary(
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn list_cloudsmith_package_versions(
    client: &reqwest::blocking::Client,
    list_url: &str,
    query: &str,
    package_name: &str,
    token: &str,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    log: &StageLogger,
) -> Result<Vec<CloudsmithVersionEntry>> {
    const PAGE_SIZE: usize = 100;
    let mut out: Vec<CloudsmithVersionEntry> = Vec::new();
    let mut page = 1u32;
    loop {
        let page_str = page.to_string();
        let page_size_str = PAGE_SIZE.to_string();
        let (_status, body) = retry_request(
            "packages/list (prune)",
            package_name,
            policy,
            deadline,
            log,
            || {
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
            },
        )?;
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
