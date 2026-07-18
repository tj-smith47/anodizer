use super::*;

/// Backend identity for a Gitea API call sequence.
///
/// Carries the HTTP client, base API URL, owner/repo coordinates, and retry
/// policy — i.e. everything that's constant for a whole release-publish
/// loop. Per-release fields (tag, name, body, …) live in
/// [`GiteaReleaseSpec`]; per-asset fields live in [`GiteaAssetSpec`].
#[derive(Clone, Copy)]
pub(crate) struct GiteaCtx<'a> {
    pub client: &'a Client,
    pub api_url: &'a str,
    pub owner: &'a str,
    pub repo: &'a str,
    pub policy: &'a RetryPolicy,
    pub deadline: Option<std::time::Instant>,
    pub log: &'a anodizer_core::log::StageLogger,
}

/// Release metadata used by [`gitea_create_release`].
#[derive(Clone, Copy)]
pub(crate) struct GiteaReleaseSpec<'a> {
    pub tag: &'a str,
    pub commit: &'a str,
    pub name: &'a str,
    pub body: &'a str,
    pub draft: bool,
    pub prerelease: bool,
    pub release_mode: &'a str,
}

/// File-on-disk identity used by every asset-upload call.
#[derive(Clone, Copy)]
pub(crate) struct GiteaAssetSpec<'a> {
    pub file_path: &'a Path,
    pub file_name: &'a str,
}
