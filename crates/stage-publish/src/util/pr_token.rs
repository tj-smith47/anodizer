//! The tokens the config names for a publisher's repository, rendered
//! before any publisher clones or pushes.

use anodizer_core::config::RepositoryConfig;

/// The token the config names for opening a pull request:
/// `pull_request.token`, else `repository.token`, each rendered through
/// [`anodizer_core::git::render_secret`] and trimmed of surrounding
/// whitespace. A link whose render is empty or only whitespace falls through
/// to the next one, and `None` leaves the choice to the environment links of
/// [`crate::util::resolve_pr_token`].
///
/// A link that fails to render is an error naming its field and the render
/// failure's cause, with the template's own text taken out: sending the
/// literal template as a bearer would fail the PR with an authentication
/// error that names nothing, and a mixed template (`ghp_…{{ Undefined }}`)
/// echoed into the error would print its literal half into the run summary.
pub(crate) fn configured_pr_token(
    repo: Option<&RepositoryConfig>,
    render: &dyn Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<Option<String>> {
    let Some(repo) = repo else {
        return Ok(None);
    };
    first_configured(
        &[
            (
                "pull_request.token",
                repo.pull_request
                    .as_ref()
                    .and_then(|pr| pr.token.as_deref()),
            ),
            ("repository.token", repo.token.as_deref()),
        ],
        render,
    )
}

/// The token the config names for pushing to a publisher's repository:
/// `repository.token`, read the way [`configured_pr_token`] reads it.
pub(crate) fn configured_push_token(
    repo: Option<&RepositoryConfig>,
    render: &dyn Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<Option<String>> {
    first_configured(
        &[("repository.token", repo.and_then(|r| r.token.as_deref()))],
        render,
    )
}

fn first_configured(
    links: &[(&str, Option<&str>)],
    render: &dyn Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<Option<String>> {
    for (field, raw) in links {
        let Some(raw) = raw else { continue };
        let token = anodizer_core::git::render_secret(field, raw, render)?;
        if !token.is_empty() {
            return Ok(Some(token));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::configured_pr_token;
    use anodizer_core::config::{PullRequestConfig, RepositoryConfig};

    /// `pull_request.token` is the first link of the configured chain and
    /// `repository.token` the second; each is rendered and trimmed, and a
    /// link whose render is empty or only whitespace falls through to the
    /// next one.
    #[test]
    fn configured_pr_token_prefers_the_pull_request_token_and_skips_a_blank_one() {
        let render = |s: &str| Ok(s.replace("{{ .Env.T }}", "rendered-token"));
        let repo = RepositoryConfig {
            token: Some("repo-token".into()),
            pull_request: Some(PullRequestConfig {
                token: Some("{{ .Env.T }}".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            configured_pr_token(Some(&repo), &render)
                .expect("renders")
                .as_deref(),
            Some("rendered-token")
        );

        let blank_pr_token = RepositoryConfig {
            pull_request: Some(PullRequestConfig {
                token: Some("".into()),
                ..Default::default()
            }),
            ..repo.clone()
        };
        assert_eq!(
            configured_pr_token(Some(&blank_pr_token), &render)
                .expect("renders")
                .as_deref(),
            Some("repo-token"),
            "a blank pull_request.token falls through to repository.token"
        );

        let spaces_pr_token = RepositoryConfig {
            pull_request: Some(PullRequestConfig {
                token: Some(" \t\n ".into()),
                ..Default::default()
            }),
            ..repo.clone()
        };
        assert_eq!(
            configured_pr_token(Some(&spaces_pr_token), &render)
                .expect("renders")
                .as_deref(),
            Some("repo-token"),
            "a pull_request.token that renders only whitespace falls through"
        );

        let padded_repo_token = RepositoryConfig {
            token: Some("  repo-token\n".into()),
            pull_request: None,
            ..Default::default()
        };
        assert_eq!(
            configured_pr_token(Some(&padded_repo_token), &render)
                .expect("renders")
                .as_deref(),
            Some("repo-token"),
            "a token is sent without its surrounding whitespace"
        );

        let nothing = RepositoryConfig {
            token: None,
            pull_request: Some(PullRequestConfig::default()),
            ..Default::default()
        };
        assert_eq!(
            configured_pr_token(Some(&nothing), &render).expect("renders"),
            None
        );
        assert_eq!(configured_pr_token(None, &render).expect("renders"), None);
    }

    /// A `pull_request.token` template the run cannot render is an error
    /// naming the field and the render failure's cause, and the template's
    /// own text is absent from it: a mixed template holding a secret-looking
    /// literal beside an undefined variable, or an unclosed placeholder whose
    /// parse error quotes the offending line, must not print the literal into
    /// the run summary. The real template layer runs over an empty variable
    /// set, where a variable nothing defined is an error. (An unset `.Env.X`
    /// renders empty by design and falls through to the next link.)
    #[test]
    fn an_unrenderable_pull_request_token_names_the_field_without_the_template_text() {
        let vars = anodizer_core::template::TemplateVars::new();
        let render = |s: &str| anodizer_core::template::render(s, &vars);
        let literal = "ghp_SecretLiteralThatMustNotLeak";
        // Both spellings of a placeholder: the Go-style `{{ .X }}` is rewritten
        // to `{{ X }}` before the engine sees it, and the engine's source
        // excerpt quotes that rewritten line.
        let pr_token_repo = |template: String| RepositoryConfig {
            token: Some("repo-token".into()),
            pull_request: Some(PullRequestConfig {
                enabled: Some(true),
                token: Some(template),
                ..Default::default()
            }),
            ..Default::default()
        };
        for (field, repo) in [
            (
                "pull_request.token",
                pr_token_repo(format!("{literal}{{{{ PrToken }}}}")),
            ),
            (
                "pull_request.token",
                pr_token_repo(format!("{literal}{{{{ .PrToken }}}}")),
            ),
            (
                "pull_request.token",
                pr_token_repo(format!("{literal}{{{{ .Env.X | nope }}}}")),
            ),
            (
                "pull_request.token",
                RepositoryConfig {
                    pull_request: Some(PullRequestConfig {
                        token: Some(format!("{literal}{{{{ PrToken")),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ),
            (
                "repository.token",
                RepositoryConfig {
                    token: Some(format!("{literal}{{{{ RepoToken }}}}")),
                    ..Default::default()
                },
            ),
        ] {
            let raw = match field {
                "pull_request.token" => repo.pull_request.as_ref().unwrap().token.clone(),
                _ => repo.token.clone(),
            }
            .unwrap();
            assert!(
                render(&raw).is_err(),
                "the fixture template {raw:?} must fail to render for the test to mean anything"
            );
            let err = configured_pr_token(Some(&repo), &render)
                .expect_err("an unrenderable token template is an error");
            let msg = format!("{err:#}");
            assert!(
                msg.starts_with(&format!("{field} does not render: ")),
                "the error names the field; got: {msg}"
            );
            assert!(
                !msg.contains(literal),
                "the template's literal must not reach the error; got: {msg}"
            );
            assert!(
                !msg.contains(&raw),
                "the template text must not reach the error; got: {msg}"
            );
            assert!(
                !msg.contains("{{"),
                "no placeholder of the template reaches the error; got: {msg}"
            );
            assert!(!msg.contains('\n'), "the cause is one line; got: {msg}");
        }
    }
}
