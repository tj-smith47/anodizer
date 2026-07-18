use super::*;

// ---------------------------------------------------------------------------
// URL-encoding aliases — consolidated onto `anodizer_core::url::percent_encode_path_segment`.
// GitLab, Gitea and GitHub all use the same strict segment set so a tag like
// `v1.0.0+build.1` produces identical URLs across backends.
// ---------------------------------------------------------------------------

pub(crate) fn encode_project_id(s: &str) -> String {
    percent_encode_path_segment(s)
}
pub(crate) fn encode_tag(s: &str) -> String {
    percent_encode_path_segment(s)
}
pub(crate) fn encode_path_segment(s: &str) -> String {
    percent_encode_path_segment(s)
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Build the GitLab project ID string from owner and name.
///
/// If `owner` is empty, only the name is returned (GitLab supports projects
/// without a namespace prefix in some API calls).
pub(crate) fn gitlab_project_id(owner: &str, name: &str) -> String {
    if owner.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", owner, name)
    }
}

/// Build the release page URL on the GitLab web UI.
pub(crate) fn gitlab_release_url(download_url: &str, owner: &str, name: &str, tag: &str) -> String {
    let base = download_url.trim_end_matches('/');
    if owner.is_empty() {
        format!("{}/{}/-/releases/{}", base, name, tag)
    } else {
        format!("{}/{}/{}/-/releases/{}", base, owner, name, tag)
    }
}
