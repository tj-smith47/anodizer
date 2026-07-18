use super::*;

/// Backend identity for a GitLab API call sequence.
///
/// Carries the HTTP client, base API URL, project_id, and retry policy — i.e.
/// everything that's constant for a whole release-publish loop. Per-release
/// fields (tag, name, body, …) live in [`GitlabReleaseSpec`]; per-asset
/// fields live in [`GitlabAssetSpec`].
#[derive(Clone, Copy)]
pub(crate) struct GitlabCtx<'a> {
    pub client: &'a Client,
    pub api_url: &'a str,
    pub project_id: &'a str,
    pub policy: &'a RetryPolicy,
    pub deadline: Option<std::time::Instant>,
    pub log: &'a anodizer_core::log::StageLogger,
}

/// Release metadata used by [`gitlab_create_release`].
#[derive(Clone, Copy)]
pub(crate) struct GitlabReleaseSpec<'a> {
    pub tag: &'a str,
    pub name: &'a str,
    pub body: &'a str,
    pub commit: &'a str,
    pub release_mode: &'a str,
}

/// File-on-disk identity used by every asset-upload call.
#[derive(Clone, Copy)]
pub(crate) struct GitlabAssetSpec<'a> {
    pub file_path: &'a Path,
    pub file_name: &'a str,
}

/// Generic Package Registry coordinates — used only when the upload path
/// is the Package Registry (PUT) rather than Project Markdown Uploads.
#[derive(Clone, Copy)]
pub(crate) struct GitlabPackageRegistrySpec<'a> {
    pub project_name: &'a str,
    pub version: &'a str,
}
