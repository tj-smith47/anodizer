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

pub(crate) use anodizer_core::gitea_url::{DEFAULT_GITEA_INSTANCE, gitea_instance_url};

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
