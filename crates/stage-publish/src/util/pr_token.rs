//! The order a publisher's repository token is read in, and the tokens the
//! config names for that repository, rendered before any publisher clones or
//! pushes.

use anodizer_core::config::RepositoryConfig;

/// One place a publisher's repository token can come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenLink {
    /// `repository.pull_request.token`, rendered.
    PullRequestToken,
    /// `repository.token`, rendered.
    RepositoryToken,
    /// The publisher's own variable, such as `HOMEBREW_TAP_TOKEN`.
    PublisherVariable,
    /// The run's token (`--token`, or the GitHub variable the CLI read at
    /// startup), read only on a GitHub run.
    RunToken,
    /// [`anodizer_core::git::GITHUB_TOKEN_ENV_LADDER`], in its order.
    GithubVariables,
}

impl TokenLink {
    /// The config field this link reads, or `None` for an environment link.
    pub(crate) fn config_field(self) -> Option<&'static str> {
        match self {
            TokenLink::PullRequestToken => Some("pull_request.token"),
            TokenLink::RepositoryToken => Some("repository.token"),
            _ => None,
        }
    }

    fn configured(self, repo: &RepositoryConfig) -> Option<&str> {
        match self {
            TokenLink::PullRequestToken => repo
                .pull_request
                .as_ref()
                .and_then(|pr| pr.token.as_deref()),
            TokenLink::RepositoryToken => repo.token.as_deref(),
            _ => None,
        }
    }
}

/// The order of the token that pushes to a publisher's repository; the
/// first link with a value wins.
pub(crate) const PUSH_TOKEN_ORDER: &[TokenLink] = &[
    TokenLink::RepositoryToken,
    TokenLink::PublisherVariable,
    TokenLink::RunToken,
    TokenLink::GithubVariables,
];

/// The order of the token that opens a pull request against a publisher's
/// upstream; the first link with a value wins.
pub(crate) const PR_TOKEN_ORDER: &[TokenLink] = &[
    TokenLink::PullRequestToken,
    TokenLink::RepositoryToken,
    TokenLink::PublisherVariable,
    TokenLink::RunToken,
    TokenLink::GithubVariables,
];

/// The token the config names for opening a pull request: the config links
/// of [`PR_TOKEN_ORDER`] (`pull_request.token`, else `repository.token`),
/// each rendered through [`anodizer_core::git::render_secret`] and trimmed of
/// surrounding whitespace. A link whose render is empty or only whitespace
/// falls through to the next one, and `None` leaves the choice to the
/// environment links of [`crate::util::resolve_pr_token`].
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
    first_configured(repo, PR_TOKEN_ORDER, render)
}

/// The token the config names for pushing to a publisher's repository: the
/// config link of [`PUSH_TOKEN_ORDER`] (`repository.token`), read the way
/// [`configured_pr_token`] reads it.
pub(crate) fn configured_push_token(
    repo: Option<&RepositoryConfig>,
    render: &dyn Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<Option<String>> {
    first_configured(repo, PUSH_TOKEN_ORDER, render)
}

fn first_configured(
    repo: Option<&RepositoryConfig>,
    order: &[TokenLink],
    render: &dyn Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<Option<String>> {
    let Some(repo) = repo else {
        return Ok(None);
    };
    for link in order {
        let (Some(field), Some(raw)) = (link.config_field(), link.configured(repo)) else {
            continue;
        };
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

    /// Each publisher page's Authentication section renders the shared
    /// `token_order` shortcode with the variable its publisher reads, and the
    /// shortcode lists the push links in [`super::PUSH_TOKEN_ORDER`] and the
    /// pull request links in [`super::PR_TOKEN_ORDER`]. No docs page writes
    /// a pull request order by hand, and every page that calls the shortcode
    /// is listed here, so a new publisher page is held to the same order.
    #[test]
    fn every_publisher_page_states_the_token_orders_the_resolvers_read() {
        use super::{PR_TOKEN_ORDER, PUSH_TOKEN_ORDER, TokenLink};
        use anodizer_core::test_helpers::docs_pages::{docs_pages, read_docs_page, repo_root};

        const PAGES: &[(&str, &str)] = &[
            ("publish/homebrew.md", "HOMEBREW_TAP_TOKEN"),
            ("publish/homebrew-casks.md", "HOMEBREW_TAP_TOKEN"),
            ("publish/krew.md", "KREW_INDEX_TOKEN"),
            ("publish/nix.md", "NIX_PKGS_TOKEN"),
            ("publish/schemastore.md", "SCHEMASTORE_TOKEN"),
            ("publish/scoop.md", "SCOOP_BUCKET_TOKEN"),
            ("publish/winget.md", "WINGET_PKGS_TOKEN"),
        ];
        let label = |link: &TokenLink, var: &str| match link {
            TokenLink::PullRequestToken => "`repository.pull_request.token`".to_string(),
            TokenLink::RepositoryToken => "`repository.token`".to_string(),
            TokenLink::PublisherVariable => format!("`{var}`"),
            TokenLink::RunToken => "the run's GitHub token".to_string(),
            TokenLink::GithubVariables => anodizer_core::git::GITHUB_TOKEN_ENV_LADDER
                .iter()
                .map(|v| format!("`{v}`"))
                .collect::<Vec<_>>()
                .join(", "),
        };
        let shortcode_path = repo_root().join("docs/site/templates/shortcodes/token_order.md");
        let shortcode = std::fs::read_to_string(&shortcode_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", shortcode_path.display()));

        for (page, var) in PAGES {
            let text = read_docs_page(page);
            let calls: Vec<&str> = text
                .lines()
                .filter(|l| l.contains("token_order("))
                .collect();
            assert_eq!(calls.len(), 1, "{page} calls token_order once: {calls:?}");
            let call = calls[0];
            assert!(
                call.contains(&format!("publisher_var=\"{var}\"")),
                "{page} passes the variable its publisher reads ({var}): {call}"
            );
            let comment =
                format!("(default: repository.token, then {var}, then the GitHub token chain)");
            for line in text
                .lines()
                .filter(|l| l.contains("token used to open the PR"))
            {
                assert!(
                    line.contains(&comment),
                    "{page}: a pull_request.token comment states the order as {comment}: {line}"
                );
            }
            let target = call
                .split("target=\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_else(|| panic!("{page} passes a target: {call}"));
            let rendered = shortcode
                .replace("{{ publisher_var }}", var)
                .replace("{{ target }}", target);
            let mut lists: Vec<Vec<String>> = Vec::new();
            let mut in_list = false;
            for line in rendered.lines() {
                let item = line
                    .split_once(". ")
                    .filter(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                    .map(|(_, item)| item.to_string());
                match item {
                    Some(item) if in_list => lists.last_mut().expect("open list").push(item),
                    Some(item) => {
                        lists.push(vec![item]);
                        in_list = true;
                    }
                    None => in_list = false,
                }
            }
            assert_eq!(
                lists.len(),
                2,
                "the shortcode holds a push and a pull request list"
            );
            for (list, order) in lists.iter().zip([PUSH_TOKEN_ORDER, PR_TOKEN_ORDER]) {
                assert_eq!(
                    list.len(),
                    order.len(),
                    "{page}: {list:?} against {order:?}"
                );
                for (item, link) in list.iter().zip(order) {
                    let want = label(link, var);
                    assert!(
                        item.starts_with(&want),
                        "{page}: the item for {link:?} reads {item:?}, expected it to start with {want}"
                    );
                }
            }
        }

        for page in docs_pages() {
            let page = page.to_str().expect("a UTF-8 page path").to_string();
            let text = read_docs_page(&page);
            if text.contains("token_order(") {
                assert!(
                    PAGES.iter().any(|(listed, _)| *listed == page),
                    "{page} calls token_order; list it here with its publisher variable"
                );
            }
            for line in text.lines() {
                let names_pr_token = line.contains("pull_request.token")
                    || line.contains("token used to open the PR");
                assert!(
                    !(names_pr_token && line.contains("ANODIZER_GITHUB_TOKEN")),
                    "{page} spells a pull request token order by hand; call token_order: {line}"
                );
            }
        }
    }
}
