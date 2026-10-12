//! The token the config names for opening a pull request, rendered once
//! before any publisher clones or pushes.

use anodizer_core::config::RepositoryConfig;

/// The token the config names for opening a pull request:
/// `pull_request.token`, else `repository.token`, each rendered. A link that
/// renders blank falls through to the next one, and `None` leaves the choice
/// to the environment chain (`anodizer_core::git::resolve_github_token_with_env`).
///
/// A link that fails to render is an error naming its field and the render
/// failure's cause, with the template's own text taken out
/// ([`token_render_cause`]): sending the literal template as a bearer would
/// fail the PR with an authentication error that names nothing, and a mixed
/// template (`ghp_…{{ Undefined }}`) echoed into the error would print its
/// literal half into the run summary.
///
/// Every publisher resolves this BEFORE its clone and push, so a template the
/// run cannot render leaves nothing on the fork
/// ([`crate::publisher_helpers::pr_token_before_push`]).
pub(crate) fn configured_pr_token(
    repo: Option<&RepositoryConfig>,
    render: &dyn Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<Option<String>> {
    let Some(repo) = repo else {
        return Ok(None);
    };
    let links = [
        (
            "pull_request.token",
            repo.pull_request
                .as_ref()
                .and_then(|pr| pr.token.as_deref()),
        ),
        ("repository.token", repo.token.as_deref()),
    ];
    for (field, raw) in links {
        let Some(raw) = raw else { continue };
        let rendered = render(raw).map_err(|e| {
            anyhow::anyhow!("{field} does not render: {}", token_render_cause(raw, &e))
        })?;
        if !rendered.is_empty() {
            return Ok(Some(rendered));
        }
    }
    Ok(None)
}

/// The root cause of a failed token render, flattened to one line, without
/// the source excerpt the template engine appends after ` --> ` and with every
/// line of the template's text replaced by `<template>`. The excerpt quotes
/// the PREPROCESSED source (`{{ .X }}` already rewritten to `{{ X }}`), so a
/// text match against the raw template alone misses it, and the render
/// layers quote the template in their context as well.
fn token_render_cause(raw: &str, err: &anyhow::Error) -> String {
    let cause = err.root_cause().to_string();
    let mut cause = cause.split(" --> ").next().unwrap_or_default().to_string();
    for part in std::iter::once(raw)
        .chain(raw.lines())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        cause = cause.replace(part, "<template>");
    }
    cause.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::configured_pr_token;
    use anodizer_core::config::{PullRequestConfig, RepositoryConfig};

    /// `pull_request.token` is the first link of the configured chain and
    /// `repository.token` the second; each is rendered, and a link that
    /// renders blank falls through to the next one.
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
