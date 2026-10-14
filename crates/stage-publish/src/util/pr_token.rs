//! The order a publisher's repository token is read in, and the tokens the
//! config names for that repository, rendered before any publisher clones or
//! pushes.

use anodizer_core::config::RepositoryConfig;
pub(crate) use anodizer_core::publish_evidence::TokenSource;

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
    /// startup, or the GitHub token file when every GitHub variable is unset),
    /// read only on a GitHub run.
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

/// [`configured_pr_token`] with the field the token came from.
pub(crate) fn configured_pr_token_tracked(
    repo: Option<&RepositoryConfig>,
    render: &dyn Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<Option<(String, TokenSource)>> {
    first_configured_tracked(repo, PR_TOKEN_ORDER, render)
}

/// [`configured_push_token`] with the field the token came from.
pub(crate) fn configured_push_token_tracked(
    repo: Option<&RepositoryConfig>,
    render: &dyn Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<Option<(String, TokenSource)>> {
    first_configured_tracked(repo, PUSH_TOKEN_ORDER, render)
}

fn first_configured(
    repo: Option<&RepositoryConfig>,
    order: &[TokenLink],
    render: &dyn Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<Option<String>> {
    Ok(first_configured_tracked(repo, order, render)?.map(|(token, _)| token))
}

fn first_configured_tracked(
    repo: Option<&RepositoryConfig>,
    order: &[TokenLink],
    render: &dyn Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<Option<(String, TokenSource)>> {
    let Some(repo) = repo else {
        return Ok(None);
    };
    for link in order {
        let (Some(field), Some(raw)) = (link.config_field(), link.configured(repo)) else {
            continue;
        };
        let token = anodizer_core::git::render_secret(field, raw, render)?;
        if !token.is_empty() {
            let source = match link {
                TokenLink::PullRequestToken => TokenSource::PullRequestField,
                _ => TokenSource::RepositoryField,
            };
            return Ok(Some((token, source)));
        }
    }
    Ok(None)
}

/// The sources a resolver reading `order` tries, in that order, for a
/// message that says what to set when none of them holds a token.
/// `config_key` prefixes the config fields (`homebrew_cores[]` renders
/// `` `homebrew_cores[].repository.token` ``), and `publisher_vars` are the
/// publisher's own variables. The run token is listed only on a GitHub run,
/// the one run whose token the resolvers read, and there its item names
/// `ANODIZER_GITHUB_TOKEN`, `GITHUB_TOKEN` and `GH_TOKEN`, which are then not
/// listed again.
pub(crate) fn token_order_hint(
    ctx: &anodizer_core::context::Context,
    order: &[TokenLink],
    config_key: &str,
    publisher_vars: &[&str],
) -> String {
    let field = |name: &str| format!("`{config_key}.{name}`");
    let vars = |names: &[&str]| {
        names
            .iter()
            .map(|n| TokenSource::EnvVar((*n).to_string()).to_string())
            .collect::<Vec<_>>()
    };
    let github_run = ctx.token_type == anodizer_core::scm::ScmTokenType::GitHub;
    let mut items: Vec<String> = Vec::new();
    for link in order {
        match link {
            TokenLink::PullRequestToken => items.push(field("repository.pull_request.token")),
            TokenLink::RepositoryToken => items.push(field("repository.token")),
            TokenLink::PublisherVariable => items.extend(vars(publisher_vars)),
            TokenLink::RunToken => {
                if github_run {
                    items.push(format!(
                        "{} (`--token`, or {} read at startup when `--token` is unset, or \
                         the GitHub token file when all three are unset)",
                        TokenSource::RunToken,
                        anodizer_core::git::GITHUB_TOKEN_ENV_LADDER.join("/"),
                    ));
                }
            }
            // On a GitHub run the run token's item already names these
            // variables: the CLI read the run token from them and then from
            // the GitHub token file, which is loaded into `GITHUB_TOKEN` only
            // when all of them are unset. So when the run token is unset they
            // are unset too.
            TokenLink::GithubVariables => {
                if !(github_run && order.contains(&TokenLink::RunToken)) {
                    items.extend(vars(anodizer_core::git::GITHUB_TOKEN_ENV_LADDER));
                }
            }
        }
    }
    items.join(", then ")
}

/// Drives a token resolver through every source it can read, for tests that
/// hold a missing-token hint to the order the resolver really reads.
#[cfg(test)]
pub(crate) mod hint_probe {
    use super::TokenSource;
    use anodizer_core::config::{PullRequestConfig, RepositoryConfig};
    use anodizer_core::context::Context;
    use anodizer_core::git::GITHUB_TOKEN_ENV_LADDER;
    use anodizer_core::scm::ScmTokenType;
    use anodizer_core::test_helpers::TestContextBuilder;

    /// A resolver under test: the context to read and the `repository:`
    /// block to hand it, giving the token it resolved.
    pub(crate) type Resolve<'a> =
        &'a dyn Fn(&mut Context, RepositoryConfig) -> anyhow::Result<Option<String>>;

    /// The sources `resolve` reads, in its order: set every source to a
    /// value naming it, resolve, record the source whose value came back,
    /// take that source away, repeat until nothing resolves.
    pub(crate) fn read_order(
        token_type: ScmTokenType,
        publisher_vars: &[&str],
        resolve: Resolve<'_>,
    ) -> Vec<TokenSource> {
        let mut left: Vec<TokenSource> =
            [TokenSource::PullRequestField, TokenSource::RepositoryField]
                .into_iter()
                .chain(std::iter::once(TokenSource::RunToken))
                .chain(
                    publisher_vars
                        .iter()
                        .chain(GITHUB_TOKEN_ENV_LADDER)
                        .map(|v| TokenSource::EnvVar((*v).to_string())),
                )
                .collect();
        let value = |s: &TokenSource| match s {
            TokenSource::PullRequestField => "pull-request-value".to_string(),
            TokenSource::RepositoryField => "repository-value".to_string(),
            TokenSource::RunToken => "run-value".to_string(),
            TokenSource::EnvVar(name) => format!("{name}-value"),
        };
        let mut order = Vec::new();
        loop {
            let mut builder = TestContextBuilder::new().sealed_env();
            let mut repo = RepositoryConfig {
                pull_request: Some(PullRequestConfig::default()),
                ..Default::default()
            };
            for source in &left {
                match source {
                    TokenSource::PullRequestField => {
                        repo.pull_request = Some(PullRequestConfig {
                            token: Some(value(source)),
                            ..Default::default()
                        });
                    }
                    TokenSource::RepositoryField => repo.token = Some(value(source)),
                    TokenSource::RunToken => builder = builder.token(Some(value(source))),
                    TokenSource::EnvVar(name) => builder = builder.env(name, value(source)),
                }
            }
            let mut ctx = builder.build();
            ctx.token_type = token_type;
            let Some(got) = resolve(&mut ctx, repo).expect("renders") else {
                return order;
            };
            let at = left
                .iter()
                .position(|s| value(s) == got)
                .unwrap_or_else(|| panic!("{got:?} is no source's value"));
            order.push(left.remove(at));
        }
    }

    /// How a hint names a source, with `config_key` before a config field.
    fn label(config_key: &str, source: &TokenSource) -> String {
        match source {
            TokenSource::PullRequestField => {
                format!("`{config_key}.repository.pull_request.token`")
            }
            TokenSource::RepositoryField => format!("`{config_key}.repository.token`"),
            other => other.to_string(),
        }
    }

    /// Asserts `hint` lists `order`, the sources a resolver read, one item per
    /// source in that order. On a GitHub run the run token's item names the
    /// GitHub variables, read after it, so each of them is named once and
    /// has no item of its own.
    pub(crate) fn assert_hint_lists(
        name: &str,
        token_type: ScmTokenType,
        hint: &str,
        order: &[TokenSource],
        config_key: &str,
    ) {
        let github = token_type == ScmTokenType::GitHub;
        assert_eq!(
            order.contains(&TokenSource::RunToken),
            github,
            "{name} on {token_type:?}: {order:?}"
        );
        let is_ladder = |s: &TokenSource| matches!(s, TokenSource::EnvVar(v) if GITHUB_TOKEN_ENV_LADDER.contains(&v.as_str()));
        let listed: Vec<&TokenSource> = if github {
            let run = order
                .iter()
                .position(|s| *s == TokenSource::RunToken)
                .expect("run token read");
            assert!(
                order[..run].iter().all(|s| !is_ladder(s)),
                "{name}: a GitHub variable is read before the run token: {order:?}"
            );
            order.iter().filter(|s| !is_ladder(s)).collect()
        } else {
            order.iter().collect()
        };
        let items: Vec<&str> = hint.split(", then ").collect();
        assert_eq!(
            items.len(),
            listed.len(),
            "{name} on {token_type:?}: the hint {hint:?} lists every source read, {order:?}"
        );
        for (item, source) in items.iter().zip(&listed) {
            assert!(
                item.starts_with(&label(config_key, source)),
                "{name} on {token_type:?}: the item {item:?} is {source:?}; hint: {hint}"
            );
        }
        for var in GITHUB_TOKEN_ENV_LADDER {
            let named = hint
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .filter(|w| w == var)
                .count();
            assert_eq!(
                named, 1,
                "{name} on {token_type:?}: {var} is named once: {hint}"
            );
        }
        if github {
            assert!(
                hint.contains(
                    "the run's GitHub token (`--token`, or \
                     ANODIZER_GITHUB_TOKEN/GITHUB_TOKEN/GH_TOKEN read at startup when \
                     `--token` is unset, or the GitHub token file when all three are unset)"
                ),
                "{name}: the run token's item names where it came from: {hint}"
            );
        }
    }
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

    /// The variables the publisher under `src/<dir>/` hands to the shared
    /// token resolvers, read from its production code: the last argument of
    /// every `resolve_pr_token`, `resolve_repo_token`, `pr_token_before_push`
    /// and `resolve_env_token_tracked` call, as a literal (`Some("X")`) or a
    /// constant of the publisher (`TOKEN_ENV_VAR`, `TOKEN_ENV_VARS`). An
    /// argument read from evidence (`t.token_env_var`) names nothing here.
    /// Every call must name the same list.
    fn publisher_token_vars(dir: &str) -> Vec<String> {
        use anodizer_core::test_helpers::test_sources::{
            function_bodies, production_half, rust_sources,
        };
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join(dir);
        let texts: Vec<String> = rust_sources(&src)
            .iter()
            .map(|f| std::fs::read_to_string(f).expect("readable source"))
            .map(|t| production_half(&t).to_string())
            .collect();
        let literals = |text: &str| -> Vec<String> {
            regex::Regex::new(r#""([A-Z0-9_]+)""#)
                .expect("valid regex")
                .captures_iter(text)
                .map(|c| c[1].to_string())
                .collect()
        };
        let constant = |name: &str| -> Option<Vec<String>> {
            let decl =
                regex::Regex::new(&format!(r"const {name}: [^=]+=([^;]+);")).expect("valid regex");
            texts
                .iter()
                .find_map(|t| decl.captures(t).map(|c| literals(&c[1])))
        };
        let call = regex::Regex::new(
            r"\b(resolve_pr_token|resolve_repo_token|pr_token_before_push|resolve_env_token_tracked)\(",
        )
        .expect("valid regex");
        let named_constant =
            regex::Regex::new(r"^(?:Some\()?&?([A-Z][A-Z0-9_]*)\)?$").expect("valid regex");
        let mut lists: std::collections::BTreeSet<Vec<String>> = Default::default();
        for text in &texts {
            for body in function_bodies(text) {
                for m in call.find_iter(&body) {
                    if body[..m.start()].ends_with("fn ") {
                        continue;
                    }
                    let mut depth = 1usize;
                    let rest = &body[m.end()..];
                    let end = rest
                        .char_indices()
                        .find(|(_, c)| {
                            match c {
                                '(' => depth += 1,
                                ')' => depth -= 1,
                                _ => {}
                            }
                            depth == 0
                        })
                        .map_or(rest.len(), |(i, _)| i);
                    let last = rest[..end].trim().trim_end_matches(',');
                    let last = last.rsplit(',').next().unwrap_or(last).trim();
                    let names = literals(last);
                    if !names.is_empty() {
                        lists.insert(names);
                    } else if let Some(name) = named_constant.captures(last) {
                        let values = constant(&name[1])
                            .unwrap_or_else(|| panic!("{dir} names {} with no constant", &name[1]));
                        lists.insert(values);
                    }
                }
            }
        }
        assert_eq!(
            lists.len(),
            1,
            "src/{dir} hands the token resolvers one list of its own variables: {lists:?}"
        );
        lists.into_iter().next().expect("one list")
    }

    /// Each publisher page's Authentication section renders the shared
    /// `token_order` shortcode with the variables its publisher's code hands
    /// the token resolvers ([`publisher_token_vars`]), and the shortcode
    /// lists the push links in [`super::PUSH_TOKEN_ORDER`] and the pull
    /// request links in [`super::PR_TOKEN_ORDER`]. No docs page writes a
    /// pull request order by hand, and every page that calls the shortcode
    /// is listed here with its publisher's source directory, so a new
    /// publisher page is held to the same order.
    #[test]
    fn every_publisher_page_states_the_token_orders_the_resolvers_read() {
        use super::{PR_TOKEN_ORDER, PUSH_TOKEN_ORDER, TokenLink};
        use anodizer_core::test_helpers::docs_pages::{docs_pages, read_docs_page, repo_root};

        const PAGES: &[(&str, &str)] = &[
            ("publish/homebrew.md", "homebrew"),
            ("publish/homebrew-casks.md", "homebrew"),
            ("publish/homebrew-core.md", "homebrew_core"),
            ("publish/krew.md", "krew"),
            ("publish/nix.md", "nix"),
            ("publish/schemastore.md", "schemastore"),
            ("publish/scoop.md", "scoop"),
            ("publish/winget.md", "winget"),
        ];
        let label = |link: &TokenLink, vars: &[String]| match link {
            TokenLink::PullRequestToken => "`repository.pull_request.token`".to_string(),
            TokenLink::RepositoryToken => "`repository.token`".to_string(),
            TokenLink::PublisherVariable => format!("`{}`", vars.join("`, then `")),
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

        for (page, dir) in PAGES {
            let vars = publisher_token_vars(dir);
            let text = read_docs_page(page);
            let calls: Vec<&str> = text
                .lines()
                .filter(|l| l.contains("token_order("))
                .collect();
            assert_eq!(calls.len(), 1, "{page} calls token_order once: {calls:?}");
            let call = calls[0];
            let quoted: Vec<String> = vars.iter().map(|v| format!("\"{v}\"")).collect();
            let passed = format!("publisher_vars=[{}]", quoted.join(", "));
            assert!(
                call.contains(&passed),
                "{page} passes the variables its publisher's code reads ({passed}): {call}"
            );
            let comment = format!(
                "(default: repository.token, then {}, then the GitHub token chain)",
                vars.join(", then ")
            );
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
                .replace(
                    "{{ publisher_vars | join(sep=\"`, then `\") }}",
                    &vars.join("`, then `"),
                )
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
                    let want = label(link, &vars);
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
