//! SCM backend abstraction: token type resolution, download URLs, and release
//! URL templates for GitHub, GitLab, and Gitea.

use crate::config::ForceTokenKind;
use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// ScmTokenType
// ---------------------------------------------------------------------------

/// Which SCM backend is in use for the current release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScmTokenType {
    GitHub,
    GitLab,
    Gitea,
}

impl fmt::Display for ScmTokenType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitHub => write!(f, "github"),
            Self::GitLab => write!(f, "gitlab"),
            Self::Gitea => write!(f, "gitea"),
        }
    }
}

// ---------------------------------------------------------------------------
// Token type resolution
// ---------------------------------------------------------------------------

/// Resolve which SCM token type to use.
///
/// Priority (highest first):
/// 1. `force_token` — explicit user config (`force_token: gitlab`)
/// 2. `detected_from_env` — inferred from environment variables present
/// 3. Default — [`ScmTokenType::GitHub`]
pub fn resolve_token_type(
    force_token: Option<&ForceTokenKind>,
    detected_from_env: Option<&str>,
) -> ScmTokenType {
    if let Some(kind) = force_token {
        return match kind {
            ForceTokenKind::GitHub => ScmTokenType::GitHub,
            ForceTokenKind::GitLab => ScmTokenType::GitLab,
            ForceTokenKind::Gitea => ScmTokenType::Gitea,
        };
    }

    if let Some(env) = detected_from_env {
        return match env.to_lowercase().as_str() {
            "gitlab" => ScmTokenType::GitLab,
            "gitea" => ScmTokenType::Gitea,
            _ => ScmTokenType::GitHub,
        };
    }

    ScmTokenType::GitHub
}

// ---------------------------------------------------------------------------
// Download URL
// ---------------------------------------------------------------------------

/// Return the base download URL for the given SCM backend.
///
/// If `custom_url` is `Some(url)` and non-empty, it takes precedence over the
/// backend default.
///
/// Defaults:
/// - GitHub: `https://github.com`
/// - GitLab: `https://gitlab.com`
/// - Gitea: `""` (must be provided by the user in config)
pub fn default_download_url(token_type: ScmTokenType, custom_url: Option<&str>) -> String {
    if let Some(url) = custom_url {
        if !url.is_empty() {
            return url.trim_end_matches('/').to_string();
        }
    }

    match token_type {
        ScmTokenType::GitHub => "https://github.com".to_string(),
        ScmTokenType::GitLab => "https://gitlab.com".to_string(),
        ScmTokenType::Gitea => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Release URL template
// ---------------------------------------------------------------------------

/// Build the release-asset download URL template for the given SCM backend.
///
/// The returned string contains Tera-style `{{ }}` placeholders that are
/// rendered later by the template engine (they are NOT Rust format strings).
///
/// - **GitHub / Gitea**: `{download}/{owner}/{name}/releases/download/{{ urlPathEscape .Tag }}/{{ .ArtifactName }}`
/// - **GitLab**: `{download}/{owner}/{name}/-/releases/{{ urlPathEscape .Tag }}/downloads/{{ .ArtifactName }}`
///   When `owner` is empty the `/{owner}` segment is omitted.
pub fn release_url_template(
    token_type: ScmTokenType,
    owner: &str,
    name: &str,
    download_url: &str,
) -> String {
    let base = download_url.trim_end_matches('/');

    match token_type {
        ScmTokenType::GitHub | ScmTokenType::Gitea => {
            format!(
                "{base}/{owner}/{name}/releases/download/{{{{ urlPathEscape .Tag }}}}/{{{{ .ArtifactName }}}}"
            )
        }
        ScmTokenType::GitLab => {
            if owner.is_empty() {
                format!(
                    "{base}/{name}/-/releases/{{{{ urlPathEscape .Tag }}}}/downloads/{{{{ .ArtifactName }}}}"
                )
            } else {
                format!(
                    "{base}/{owner}/{name}/-/releases/{{{{ urlPathEscape .Tag }}}}/downloads/{{{{ .ArtifactName }}}}"
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Display -----------------------------------------------------------

    #[test]
    fn display_github() {
        assert_eq!(ScmTokenType::GitHub.to_string(), "github");
    }

    #[test]
    fn display_gitlab() {
        assert_eq!(ScmTokenType::GitLab.to_string(), "gitlab");
    }

    #[test]
    fn display_gitea() {
        assert_eq!(ScmTokenType::Gitea.to_string(), "gitea");
    }

    // -- resolve_token_type ------------------------------------------------

    #[test]
    fn resolve_force_github() {
        let result = resolve_token_type(Some(&ForceTokenKind::GitHub), None);
        assert_eq!(result, ScmTokenType::GitHub);
    }

    #[test]
    fn resolve_force_gitlab() {
        let result = resolve_token_type(Some(&ForceTokenKind::GitLab), None);
        assert_eq!(result, ScmTokenType::GitLab);
    }

    #[test]
    fn resolve_force_gitea() {
        let result = resolve_token_type(Some(&ForceTokenKind::Gitea), None);
        assert_eq!(result, ScmTokenType::Gitea);
    }

    #[test]
    fn resolve_force_overrides_env() {
        // force_token wins even if env says something different
        let result = resolve_token_type(Some(&ForceTokenKind::Gitea), Some("gitlab"));
        assert_eq!(result, ScmTokenType::Gitea);
    }

    #[test]
    fn resolve_env_gitlab() {
        let result = resolve_token_type(None, Some("gitlab"));
        assert_eq!(result, ScmTokenType::GitLab);
    }

    #[test]
    fn resolve_env_gitea() {
        let result = resolve_token_type(None, Some("gitea"));
        assert_eq!(result, ScmTokenType::Gitea);
    }

    #[test]
    fn resolve_env_github() {
        let result = resolve_token_type(None, Some("github"));
        assert_eq!(result, ScmTokenType::GitHub);
    }

    #[test]
    fn resolve_env_case_insensitive() {
        assert_eq!(
            resolve_token_type(None, Some("GitLab")),
            ScmTokenType::GitLab
        );
        assert_eq!(
            resolve_token_type(None, Some("GITEA")),
            ScmTokenType::Gitea
        );
    }

    #[test]
    fn resolve_env_unknown_falls_back_to_github() {
        let result = resolve_token_type(None, Some("bitbucket"));
        assert_eq!(result, ScmTokenType::GitHub);
    }

    #[test]
    fn resolve_default_is_github() {
        let result = resolve_token_type(None, None);
        assert_eq!(result, ScmTokenType::GitHub);
    }

    // -- default_download_url ---------------------------------------------

    #[test]
    fn download_url_github_default() {
        let url = default_download_url(ScmTokenType::GitHub, None);
        assert_eq!(url, "https://github.com");
    }

    #[test]
    fn download_url_gitlab_default() {
        let url = default_download_url(ScmTokenType::GitLab, None);
        assert_eq!(url, "https://gitlab.com");
    }

    #[test]
    fn download_url_gitea_default_is_empty() {
        let url = default_download_url(ScmTokenType::Gitea, None);
        assert_eq!(url, "");
    }

    #[test]
    fn download_url_custom_overrides_default() {
        let url = default_download_url(ScmTokenType::GitHub, Some("https://gh.corp.com"));
        assert_eq!(url, "https://gh.corp.com");
    }

    #[test]
    fn download_url_custom_trailing_slash_stripped() {
        let url = default_download_url(ScmTokenType::GitLab, Some("https://gl.corp.com/"));
        assert_eq!(url, "https://gl.corp.com");
    }

    #[test]
    fn download_url_empty_custom_falls_back_to_default() {
        let url = default_download_url(ScmTokenType::GitHub, Some(""));
        assert_eq!(url, "https://github.com");
    }

    // -- release_url_template ---------------------------------------------

    #[test]
    fn release_template_github() {
        let tpl = release_url_template(
            ScmTokenType::GitHub,
            "owner",
            "repo",
            "https://github.com",
        );
        assert_eq!(
            tpl,
            "https://github.com/owner/repo/releases/download/{{ urlPathEscape .Tag }}/{{ .ArtifactName }}"
        );
    }

    #[test]
    fn release_template_gitea() {
        let tpl = release_url_template(
            ScmTokenType::Gitea,
            "myorg",
            "myapp",
            "https://gitea.example.com",
        );
        assert_eq!(
            tpl,
            "https://gitea.example.com/myorg/myapp/releases/download/{{ urlPathEscape .Tag }}/{{ .ArtifactName }}"
        );
    }

    #[test]
    fn release_template_gitlab() {
        let tpl = release_url_template(
            ScmTokenType::GitLab,
            "group",
            "project",
            "https://gitlab.com",
        );
        assert_eq!(
            tpl,
            "https://gitlab.com/group/project/-/releases/{{ urlPathEscape .Tag }}/downloads/{{ .ArtifactName }}"
        );
    }

    #[test]
    fn release_template_gitlab_empty_owner() {
        let tpl = release_url_template(ScmTokenType::GitLab, "", "project", "https://gitlab.com");
        assert_eq!(
            tpl,
            "https://gitlab.com/project/-/releases/{{ urlPathEscape .Tag }}/downloads/{{ .ArtifactName }}"
        );
    }

    #[test]
    fn release_template_trailing_slash_stripped() {
        let tpl = release_url_template(
            ScmTokenType::GitHub,
            "o",
            "r",
            "https://github.com/",
        );
        assert_eq!(
            tpl,
            "https://github.com/o/r/releases/download/{{ urlPathEscape .Tag }}/{{ .ArtifactName }}"
        );
    }
}
