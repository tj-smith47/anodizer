use super::*;

/// Build the release page URL on the Gitea web UI.
///
/// Returns `{download}/{owner}/{repo}/releases/tag/{tag}`.
pub(crate) fn gitea_release_url(download_url: &str, owner: &str, repo: &str, tag: &str) -> String {
    let base = download_url.trim_end_matches('/');
    format!(
        "{}/{}/{}/releases/tag/{}",
        base,
        encode_segment(owner),
        encode_segment(repo),
        encode_segment(tag)
    )
}

/// The Gitea instance root behind a configured `gitea_urls.api`.
///
/// Every request builder appends its own `/api/v1/…`, so a value that already
/// ends in `/api/v1` — the form Gitea's own API docs hand out — would send
/// `/api/v1/api/v1/…` and 404. Exactly one trailing slash and exactly one
/// terminal `/api/v1` are trimmed, so a self-hosted deployment subpath
/// (`https://example.com/forge`) survives.
pub(crate) fn gitea_instance_url(api: &str) -> &str {
    let trimmed = api.strip_suffix('/').unwrap_or(api);
    trimmed.strip_suffix("/api/v1").unwrap_or(trimmed)
}
