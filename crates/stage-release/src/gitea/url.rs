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

/// The built-in Gitea instance — used when `gitea_urls.api` / `.download` are
/// unset. Named once so the default cannot drift between the two fields and
/// the tests that pin them.
pub(crate) const DEFAULT_GITEA_INSTANCE: &str = "https://gitea.com";

/// Whether a configured base URL carries both a scheme and a host.
///
/// Mirrors Gitea's own client construction, which rejects a parsed URL whose
/// `Scheme` or `Host` is empty: either half missing builds a relative or
/// hostless request URL that fails far from the config that caused it —
/// `https:///forge` has a scheme and no host, and `gitea.example.com` has a
/// host-shaped value and no scheme.
pub(crate) fn has_scheme_and_host(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !scheme.is_empty() && !host.is_empty()
}
