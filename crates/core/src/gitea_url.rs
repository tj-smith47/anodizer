//! Gitea instance-root resolution, shared by every consumer of
//! `gitea_urls.api`.
//!
//! `gitea_urls.api` is documented as the INSTANCE root
//! (`https://gitea.mycompany.com`), and each request builder appends its own
//! `/api/v1/…`. Gitea's own API docs hand out the `/api/v1` form, so both
//! spellings reach the config and both must build the same request URL.

/// The built-in Gitea instance — used when `gitea_urls.api` / `.download` are
/// unset. Named once so the default cannot drift between the two fields and
/// the tests that pin them.
pub const DEFAULT_GITEA_INSTANCE: &str = "https://gitea.com";

/// The Gitea instance root behind a configured `gitea_urls.api`.
///
/// Exactly one trailing slash and exactly one terminal `/api/v1` are trimmed,
/// so a self-hosted deployment subpath (`https://example.com/forge`) survives.
pub fn gitea_instance_url(api: &str) -> &str {
    let trimmed = api.strip_suffix('/').unwrap_or(api);
    trimmed.strip_suffix("/api/v1").unwrap_or(trimmed)
}

/// The `/api/v1` base a REST request is built on, from an optional configured
/// `gitea_urls.api`. `None` falls back to the built-in instance.
pub fn gitea_api_base(api: Option<&str>) -> String {
    let configured = api.unwrap_or(DEFAULT_GITEA_INSTANCE);
    format!("{}/api/v1", gitea_instance_url(configured))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_instance_root_and_the_api_v1_form_build_one_request_url() {
        // Both spellings are in the wild: the docs show the instance root, and
        // Gitea's own API docs hand out the `/api/v1` form.
        assert_eq!(
            gitea_api_base(Some("https://gitea.example.com")),
            "https://gitea.example.com/api/v1"
        );
        assert_eq!(
            gitea_api_base(Some("https://gitea.example.com/api/v1")),
            "https://gitea.example.com/api/v1"
        );
        assert_eq!(
            gitea_api_base(Some("https://gitea.example.com/api/v1/")),
            "https://gitea.example.com/api/v1"
        );
    }

    #[test]
    fn a_deployment_subpath_survives_the_trim() {
        assert_eq!(
            gitea_instance_url("https://example.com/forge"),
            "https://example.com/forge"
        );
        assert_eq!(
            gitea_api_base(Some("https://example.com/forge/api/v1")),
            "https://example.com/forge/api/v1"
        );
    }

    #[test]
    fn an_unset_api_falls_back_to_the_built_in_instance() {
        assert_eq!(gitea_api_base(None), "https://gitea.com/api/v1");
        assert_eq!(
            gitea_instance_url(DEFAULT_GITEA_INSTANCE),
            DEFAULT_GITEA_INSTANCE
        );
    }
}
