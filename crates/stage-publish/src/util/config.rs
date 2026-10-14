//! Config / context lookups & resolution helpers shared across publishers.
//!
//! - Publisher config lookup (`get_publish_config`).
//! - Artifact-kind resolution (`resolve_artifact_kind`).
//! - Secret rendering (`render_secret`, `resolve_secret_name`) and token
//!   resolution (`resolve_token`,
//!   `resolve_repo_token`, `resolve_pr_token`).
//! - Repository owner/name extraction (`resolve_repo_owner_name`).
//! - Skip-gate evaluation (`should_skip_upload`, `should_skip_publisher_with_if`).

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{CrateConfig, PublishConfig, is_false_spelling, is_true_spelling};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

/// Render the credential template `raw` of the config field `field` (an
/// `api_key`, a `token`, a `password`, a `private_key`, a signing key, or a
/// `secret_name` that names the variable holding one) and return it trimmed.
///
/// A template that fails to render is an error in every mode, strict or not,
/// naming `field` without the template's text
/// ([`anodizer_core::git::render_secret`]): a literal template sent as a
/// credential fails upstream with an error that names nothing, and on a
/// one-way-door registry it is sent before anything can stop it.
pub(crate) fn render_secret(ctx: &Context, field: &str, raw: &str) -> Result<String> {
    ctx.render_secret(field, raw)
}

/// Resolve the name of the variable that holds a secret: the configured
/// `secret_name` (rendered through [`render_secret`]), else `default`.
pub(crate) fn resolve_secret_name(
    ctx: &Context,
    field: &str,
    secret_name: Option<&str>,
    default: &str,
) -> Result<String> {
    match secret_name {
        Some(name) => render_secret(ctx, field, name),
        None => Ok(default.to_string()),
    }
}

/// Look up a crate's config and its `publish` section by name, returning a
/// descriptive error when either is missing.
///
/// Resolves against the full crate universe
/// ([`anodizer_core::config::Config::crate_universe`]) — top-level
/// `ctx.config.crates` PLUS every `ctx.config.workspaces[].crates` — so a
/// workspace-only crate (the only shape in a pure-workspace config like a
/// multi-crate monorepo) is found by name. Without the workspace
/// fallthrough every per-publisher lookup (nix, homebrew, scoop, aur, krew,
/// winget, chocolatey) — and the snapshot emission validator that drives
/// them — would `bail!` "not found" for a crate defined under `workspaces:`.
pub(crate) fn get_publish_config<'a>(
    ctx: &'a Context,
    crate_name: &str,
    label: &str,
) -> Result<(&'a CrateConfig, &'a PublishConfig)> {
    let crate_cfg = find_crate_in_universe(ctx, crate_name)
        .ok_or_else(|| anyhow::anyhow!("{label}: crate '{crate_name}' not found in config"))?;

    let publish = crate_cfg
        .publish
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{label}: no publish config for '{crate_name}'"))?;

    Ok((crate_cfg, publish))
}

/// Borrow a crate by name from the full crate universe
/// ([`anodizer_core::config::Config::crate_universe`] — top-level wins on a
/// name collision). Returns a reference tied to `ctx` so callers can keep
/// zero-copy `&CrateConfig` access without cloning the whole universe.
pub(crate) fn find_crate_in_universe<'a>(
    ctx: &'a Context,
    crate_name: &str,
) -> Option<&'a CrateConfig> {
    ctx.config.find_crate(crate_name)
}

/// Map the `use` config value (e.g. "archive", "msi", "nsis") to an
/// `ArtifactKind`.  Defaults to `Archive` when the value is `None` or
/// unrecognised.
pub(crate) fn resolve_artifact_kind(use_value: Option<&str>) -> ArtifactKind {
    match use_value {
        Some("msi") | Some("nsis") => ArtifactKind::Installer,
        // "archive" or anything else defaults to Archive
        _ => ArtifactKind::Archive,
    }
}

/// Resolve a GitHub token from the environment for a publisher's repository:
/// the publisher-specific env var, then the run's GitHub token (`--token`, or
/// the value the CLI resolved from `ANODIZER_GITHUB_TOKEN` / `GITHUB_TOKEN` /
/// `GH_TOKEN` at startup), then `ANODIZER_GITHUB_TOKEN`, `GITHUB_TOKEN` and
/// `GH_TOKEN`. Every value is trimmed, and one that is empty or only
/// whitespace falls through ([`anodizer_core::git::non_blank_token`]).
///
/// The publisher variable comes first because it names a token issued for
/// that one repository; the run token is filled from the generic GitHub
/// variables before any publisher runs, so reading it first would leave the
/// publisher variable unreachable whenever a GitHub token is set. In a
/// GitLab or Gitea run the run token is a GitLab or Gitea credential and is
/// never sent to a GitHub repository.
///
/// Env reads route through the [`Context`]'s injected env source so unit
/// tests can drive the fallback ladder via a
/// [`MapEnvSource`](anodizer_core::MapEnvSource) without mutating the
/// process env.
pub(crate) fn resolve_token(ctx: &Context, env_var: Option<&str>) -> Option<String> {
    resolve_env_token_tracked(ctx, env_var.as_slice()).map(|(token, _)| token)
}

/// [`resolve_token`] for a publisher with more than one variable of its own
/// (`env_vars`, read in order), returning the token with the variable it came
/// from: `None` for the run token, which no single variable holds. A
/// publisher that records the variable in its rollback evidence reads it
/// back from here.
pub(crate) fn resolve_env_token_tracked(
    ctx: &Context,
    env_vars: &[&str],
) -> Option<(String, Option<String>)> {
    use super::pr_token::{PUSH_TOKEN_ORDER, TokenLink};
    let first_set = |names: &[&str]| {
        names.iter().find_map(|name| {
            ctx.env_var(name)
                .as_deref()
                .and_then(anodizer_core::git::non_blank_token)
                .map(|token| (token, Some((*name).to_string())))
        })
    };
    PUSH_TOKEN_ORDER.iter().find_map(|link| match link {
        TokenLink::PublisherVariable => first_set(env_vars),
        TokenLink::RunToken => run_github_token(ctx).map(|token| (token, None)),
        TokenLink::GithubVariables => first_set(anodizer_core::git::GITHUB_TOKEN_ENV_LADDER),
        TokenLink::PullRequestToken | TokenLink::RepositoryToken => None,
    })
}

/// The run's SCM token when it is a GitHub credential, else `None`.
pub(crate) fn run_github_token(ctx: &Context) -> Option<String> {
    if ctx.token_type != anodizer_core::scm::ScmTokenType::GitHub {
        return None;
    }
    ctx.options
        .token
        .as_deref()
        .and_then(anodizer_core::git::non_blank_token)
}

/// Resolve repository owner/name from a RepositoryConfig.
///
/// Returns `Some((owner, name))` when both fields are populated, `None`
/// when neither is set or only one is set. Callers chain `.ok_or_else()`
/// to report a missing-repository error.
pub(crate) fn resolve_repo_owner_name(
    repo: Option<&anodizer_core::config::RepositoryConfig>,
) -> Option<(String, String)> {
    repo.and_then(|r| match (r.owner.as_deref(), r.name.as_deref()) {
        (Some(o), Some(n)) => Some((o.to_string(), n.to_string())),
        _ => None,
    })
}

/// Resolve `skip_upload` to a boolean for publisher entry-points, emitting
/// the canonical skip-upload log line when the upload is skipped.
///
/// Accepts the StringOrBool field directly. Templates are rendered via
/// `ctx.render_template`. Returns:
/// - `true` if the value renders to literal "true", or to "auto" while the
///   release is a prerelease (template var `Prerelease` is non-empty).
/// - `false` if `None`, or renders to "false"/"".
/// - `false` with a warn if the value is an unrecognized string.
///
/// When the decision is `true` and `label` is `Some`, this emits
/// [`skip_upload_line`] (the same phrasing [`should_skip_publisher_with_if`]
/// uses). Callers pass `None` on render and validation paths, which
/// re-render the same artifact the publisher reports on and so must not
/// print the skip line a second time; an unrecognized value still warns on
/// `log`.
pub(crate) fn should_skip_upload(
    skip_upload: Option<&anodizer_core::config::StringOrBool>,
    ctx: &Context,
    log: &StageLogger,
    label: Option<&str>,
) -> Result<bool> {
    let raw = match skip_upload {
        Some(v) => v.as_str(),
        None => return Ok(false),
    };
    let rendered = super::template::render_or_warn(ctx, log, "skip_upload", raw)?;
    let value = rendered.trim();
    let skip = if is_true_spelling(value) {
        true
    } else if is_false_spelling(value) || value.is_empty() {
        false
    } else if value.eq_ignore_ascii_case("auto") {
        let pre = ctx
            .template_vars()
            .get("Prerelease")
            .cloned()
            .unwrap_or_default();
        !pre.is_empty()
    } else {
        log.warn(&format!(
            "unrecognized skip_upload value {value:?} (expected \"true\", \"false\", \"1\", \"0\" or \"auto\"); treating as false"
        ));
        false
    };
    if skip && let Some(label) = label {
        log.status(&skip_upload_line(label));
    }
    Ok(skip)
}

/// The status line a publisher prints for an entry its `skip_upload` skips.
pub(crate) fn skip_upload_line(label: &str) -> String {
    format!("skipped {label} upload — skip_upload is set")
}

/// Evaluate `skip` / `skip_upload` / `if:` fields for a publisher entry.
///
/// Returns `Ok(true)` to skip; `Ok(false)` to proceed, so every publisher
/// entry reads its three gates in one order with one wording.
/// `if_condition` is the resource's `if:` template (or `None` when the
/// resource does not yet expose `if:`).
pub(crate) fn should_skip_publisher_with_if(
    ctx: &Context,
    skip: Option<&anodizer_core::config::StringOrBool>,
    skip_upload: Option<&anodizer_core::config::StringOrBool>,
    if_condition: Option<&str>,
    label: &str,
    log: &StageLogger,
) -> Result<bool> {
    if let Some(d) = skip {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("{label}: render skip template"))?;
        if off {
            log.status(&format!(
                "skipped {label} — `skip` condition evaluated truthy"
            ));
            return Ok(true);
        }
    }
    // `skip_upload` honors the `auto` value (skip for prereleases) via the
    // shared `should_skip_upload`. A bare `try_evaluates_to_true` would
    // silently treat `auto` as an unknown string and never skip a
    // prerelease, regressing the documented `skip_upload: auto` semantics.
    if skip_upload.is_some() && should_skip_upload(skip_upload, ctx, log, Some(label))? {
        return Ok(true);
    }
    let proceed = anodizer_core::config::evaluate_if_condition(if_condition, label, |t| {
        ctx.render_template(t)
    })?;
    if !proceed {
        log.status(&format!("skipped {label} — `if` condition evaluated falsy"));
        return Ok(true);
    }
    Ok(false)
}

/// Resolve the token that pushes to a publisher's GitHub repository in
/// [`PUSH_TOKEN_ORDER`](super::pr_token::PUSH_TOKEN_ORDER):
/// `repository.token`, then the publisher's own variable `env_var`, then the
/// run token (GitHub only), then `ANODIZER_GITHUB_TOKEN` / `GITHUB_TOKEN` /
/// `GH_TOKEN` ([`resolve_token`]).
///
/// `repository.token` is rendered through [`render_secret`]: a template that
/// fails to render is an error, and one that renders empty or only
/// whitespace falls through to the next source. `Ok(None)` when no source
/// has a value.
pub(crate) fn resolve_repo_token(
    ctx: &Context,
    repo: Option<&anodizer_core::config::RepositoryConfig>,
    env_var: Option<&str>,
) -> Result<Option<String>> {
    let configured = super::pr_token::configured_push_token(repo, &|s| ctx.render_template(s))?;
    Ok(configured.or_else(|| resolve_token(ctx, env_var)))
}

/// Resolve the token that opens a pull request against a publisher's
/// upstream in [`PR_TOKEN_ORDER`](super::pr_token::PR_TOKEN_ORDER):
/// `pull_request.token`, then the chain of [`resolve_repo_token`]
/// (`repository.token`, the publisher's own variable, the run token on a
/// GitHub run, then `ANODIZER_GITHUB_TOKEN` / `GITHUB_TOKEN` / `GH_TOKEN`).
///
/// Every PR path resolves its token here, and preflight probes the same
/// value, so a publisher variable holding a personal access token reaches
/// the pull request as well as the push.
pub(crate) fn resolve_pr_token(
    ctx: &Context,
    repo: Option<&anodizer_core::config::RepositoryConfig>,
    env_var: Option<&str>,
) -> Result<Option<String>> {
    let configured = super::pr_token::configured_pr_token(repo, &|s| ctx.render_template(s))?;
    Ok(configured.or_else(|| resolve_token(ctx, env_var)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{
        BinstallConfig, CrateConfig, NixConfig, PublishConfig, RepositoryConfig, WorkspaceConfig,
    };
    use anodizer_core::test_helpers::TestContextBuilder;

    /// A set-but-blank `GITHUB_TOKEN=""` (GitHub Actions' shape for a missing
    /// secret) or one holding only whitespace falls through to the next
    /// variable, and a padded value is sent trimmed.
    #[test]
    fn a_blank_variable_falls_through_and_a_padded_one_is_trimmed() {
        let ctx = TestContextBuilder::new()
            .sealed_env()
            .env("MY_TOKEN", " \t")
            .env("ANODIZER_GITHUB_TOKEN", "")
            .env("GITHUB_TOKEN", "  real\n")
            .build();
        assert_eq!(
            resolve_token(&ctx, Some("MY_TOKEN")).as_deref(),
            Some("real")
        );
        let none = TestContextBuilder::new()
            .sealed_env()
            .env("GITHUB_TOKEN", "")
            .build();
        assert_eq!(resolve_token(&none, None), None);
    }

    /// A padded run token is sent trimmed, and one holding only whitespace
    /// falls through to the environment chain.
    #[test]
    fn the_run_token_is_trimmed() {
        let padded = TestContextBuilder::new()
            .sealed_env()
            .token(Some(" run\t".into()))
            .build();
        assert_eq!(resolve_token(&padded, None).as_deref(), Some("run"));
        let blank = TestContextBuilder::new()
            .sealed_env()
            .env("GH_TOKEN", "gh")
            .token(Some("  ".into()))
            .build();
        assert_eq!(resolve_token(&blank, None).as_deref(), Some("gh"));
    }

    /// A templated `repository.token` (`{{ .Env.GH_PAT }}`) must be rendered
    /// to the env value before it is used as the auth credential — the
    /// literal template string must never become the bearer token.
    #[test]
    fn resolve_repo_token_renders_templated_token() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.template_vars_mut().set_env("GH_PAT", "ghp_real_value");
        let repo = RepositoryConfig {
            token: Some("{{ .Env.GH_PAT }}".into()),
            ..Default::default()
        };
        let tok = resolve_repo_token(&ctx, Some(&repo), None).unwrap();
        assert_eq!(tok.as_deref(), Some("ghp_real_value"));
        assert!(
            !tok.unwrap().contains("{{"),
            "the literal template must never become the auth token"
        );
    }

    /// A templated `secret_name` (`TOKEN_{{ .Env.X }}`) must render to the
    /// env-var name BEFORE the lookup, identically regardless of which
    /// publisher's default is supplied. cloudsmith already routed through this
    /// SSOT, and so do dockerhub and gemfury: a `secret_name:
    /// "TOKEN_{{ .Env.X }}"` with `.Env.X=FOO` resolves to `TOKEN_FOO` for all
    /// three, and the literal template string is never looked up.
    #[test]
    fn resolve_secret_name_renders_templated_value_identically_across_publishers() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.template_vars_mut().set_env("X", "FOO");
        let templated = Some("TOKEN_{{ .Env.X }}");
        // The default differs per publisher but the render path is the same;
        // a present (templated) `secret_name` ignores the default entirely.
        let name = |default| resolve_secret_name(&ctx, "secret_name", templated, default).unwrap();
        let cloudsmith = name("CLOUDSMITH_TOKEN");
        let dockerhub = name("DOCKER_PASSWORD");
        let gemfury = name("FURY_PUSH_TOKEN");
        assert_eq!(cloudsmith, "TOKEN_FOO");
        assert_eq!(dockerhub, "TOKEN_FOO");
        assert_eq!(gemfury, "TOKEN_FOO");
        assert!(
            !cloudsmith.contains("{{"),
            "the literal template must never become the env-var name"
        );
    }

    /// A plain (non-templated) `repository.token` passes through unchanged.
    #[test]
    fn resolve_repo_token_returns_plain_token_verbatim() {
        let ctx = TestContextBuilder::new().build();
        let repo = RepositoryConfig {
            token: Some("ghp_literal".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_repo_token(&ctx, Some(&repo), None)
                .unwrap()
                .as_deref(),
            Some("ghp_literal")
        );
    }

    /// A crate carrying a `publish.nix` block plus an enabled binstall
    /// emission — the shape `get_publish_config` and the snapshot validator
    /// must resolve by name.
    fn nix_binstall_crate(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            binstall: Some(BinstallConfig {
                enabled: Some(true),
                ..Default::default()
            }),
            publish: Some(PublishConfig {
                nix: Some(NixConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn get_publish_config_resolves_workspace_only_crate() {
        // A pure-workspace config (no top-level `crates:`) is cfgd's shape:
        // every crate lives under `workspaces:`. Before the universe-aware
        // lookup, `get_publish_config` searched only `ctx.config.crates`
        // (empty here) and bailed "crate 'cfgd' not found in config",
        // breaking emission-validate for nix/binstall/homebrew/scoop/...
        let ctx = TestContextBuilder::new()
            .workspaces(vec![WorkspaceConfig {
                name: "cfgd".to_string(),
                crates: vec![nix_binstall_crate("cfgd")],
                ..Default::default()
            }])
            .build();

        let (crate_cfg, publish) =
            get_publish_config(&ctx, "cfgd", "nix").expect("workspace crate must resolve by name");
        assert_eq!(crate_cfg.name, "cfgd");
        assert!(publish.nix.is_some(), "nix block must be reachable");
        assert!(
            crate_cfg
                .binstall
                .as_ref()
                .is_some_and(|b| b.enabled == Some(true)),
            "binstall block must be reachable on the resolved crate"
        );
    }

    #[test]
    fn get_publish_config_top_level_wins_over_workspace_on_name_collision() {
        // Mirrors `crate_universe` precedence: a top-level entry shadows a
        // workspace entry sharing its name.
        let mut top = nix_binstall_crate("dup");
        top.path = "crates/top".to_string();
        let mut ws = nix_binstall_crate("dup");
        ws.path = "ws/dup".to_string();
        let ctx = TestContextBuilder::new()
            .crates(vec![top])
            .workspaces(vec![WorkspaceConfig {
                name: "ws-a".to_string(),
                crates: vec![ws],
                ..Default::default()
            }])
            .build();

        let (crate_cfg, _publish) =
            get_publish_config(&ctx, "dup", "nix").expect("collision must resolve");
        assert_eq!(
            crate_cfg.path, "crates/top",
            "top-level entry must win on name collision"
        );
    }

    #[test]
    fn get_publish_config_resolves_crate_in_a_later_workspace() {
        // Multi-workspace monorepo: the target crate lives in the SECOND
        // workspace. The lookup flat_maps across every `workspaces[]` entry,
        // so a crate is found regardless of which workspace declares it — a
        // search that stopped at `workspaces[0]` would miss it here.
        let ctx = TestContextBuilder::new()
            .workspaces(vec![
                WorkspaceConfig {
                    name: "first".to_string(),
                    crates: vec![nix_binstall_crate("alpha")],
                    ..Default::default()
                },
                WorkspaceConfig {
                    name: "second".to_string(),
                    crates: vec![nix_binstall_crate("omega")],
                    ..Default::default()
                },
            ])
            .build();

        let (crate_cfg, publish) = get_publish_config(&ctx, "omega", "nix")
            .expect("crate in a later workspace must resolve by name");
        assert_eq!(crate_cfg.name, "omega");
        assert!(publish.nix.is_some(), "nix block must be reachable");
    }

    #[test]
    fn get_publish_config_unknown_crate_still_errors() {
        let ctx = TestContextBuilder::new()
            .workspaces(vec![WorkspaceConfig {
                name: "ws-a".to_string(),
                crates: vec![nix_binstall_crate("present")],
                ..Default::default()
            }])
            .build();
        let err = get_publish_config(&ctx, "absent", "nix").expect_err("must error");
        assert!(
            err.to_string().contains("not found in config"),
            "missing crate must still bail: {err}"
        );
    }
    /// A publisher's own token variable wins over the run token the CLI
    /// filled from `GITHUB_TOKEN` at startup.
    #[test]
    fn the_publisher_variable_wins_over_the_run_token() {
        let ctx = TestContextBuilder::new()
            .sealed_env()
            .env("GITHUB_TOKEN", "run")
            .env("SCOOP_BUCKET_TOKEN", "bucket")
            .token(Some("run".into()))
            .build();
        assert_eq!(
            resolve_token(&ctx, Some("SCOOP_BUCKET_TOKEN")).as_deref(),
            Some("bucket")
        );
        assert_eq!(resolve_token(&ctx, None).as_deref(), Some("run"));
    }

    /// In a GitLab or Gitea run the run token is not a GitHub credential, so
    /// it is never sent to a GitHub repository.
    #[test]
    fn a_gitlab_or_gitea_run_token_is_not_used_for_a_github_repository() {
        for token_type in [
            anodizer_core::scm::ScmTokenType::GitLab,
            anodizer_core::scm::ScmTokenType::Gitea,
        ] {
            let mut ctx = TestContextBuilder::new()
                .sealed_env()
                .token(Some("glpat".into()))
                .build();
            ctx.token_type = token_type;
            assert_eq!(resolve_token(&ctx, None), None);
            let ctx_with_gh = {
                let mut c = TestContextBuilder::new()
                    .sealed_env()
                    .env("GH_TOKEN", "gh")
                    .token(Some("glpat".into()))
                    .build();
                c.token_type = token_type;
                c
            };
            assert_eq!(resolve_token(&ctx_with_gh, None).as_deref(), Some("gh"));
        }
    }

    /// A `repository.token` that does not render is an error that names the
    /// field and leaves out the template's text, in every mode: the template is
    /// never sent as the credential, and the publisher variable is not used
    /// in its place.
    #[test]
    fn a_repository_token_that_does_not_render_is_an_error() {
        let ctx = TestContextBuilder::new()
            .sealed_env()
            .env("KREW_INDEX_TOKEN", "krew")
            .build();
        let repo = RepositoryConfig {
            token: Some("ghp_secret{{ NoSuchVariable }}".into()),
            ..Default::default()
        };
        let err = resolve_repo_token(&ctx, Some(&repo), Some("KREW_INDEX_TOKEN"))
            .expect_err("a token that does not render must not resolve");
        let msg = format!("{err:#}");
        assert!(msg.contains("repository.token"), "{msg}");
        assert!(!msg.contains("ghp_secret"), "{msg}");
        let err = resolve_pr_token(&ctx, Some(&repo), Some("KREW_INDEX_TOKEN"))
            .expect_err("the pull-request chain reads the same token");
        assert!(format!("{err:#}").contains("repository.token"));
    }

    /// A publisher variable holding a personal access token opens the pull
    /// request as well as pushing the fork, when the Actions `GITHUB_TOKEN`
    /// is the run token.
    #[test]
    fn the_publisher_variable_opens_the_pull_request_over_the_run_token() {
        let ctx = TestContextBuilder::new()
            .sealed_env()
            .env("GITHUB_TOKEN", "ghs_actions")
            .env("WINGET_PKGS_TOKEN", "ghp_pat")
            .token(Some("ghs_actions".into()))
            .build();
        let repo = RepositoryConfig::default();
        assert_eq!(
            resolve_pr_token(&ctx, Some(&repo), Some("WINGET_PKGS_TOKEN"))
                .unwrap()
                .as_deref(),
            Some("ghp_pat")
        );
        assert_eq!(
            resolve_repo_token(&ctx, Some(&repo), Some("WINGET_PKGS_TOKEN"))
                .unwrap()
                .as_deref(),
            Some("ghp_pat")
        );
    }

    /// `pull_request.token` wins over `repository.token` for the pull request
    /// only; the push still uses `repository.token`.
    #[test]
    fn the_pull_request_token_wins_for_the_pull_request_only() {
        let ctx = TestContextBuilder::new().sealed_env().build();
        let repo = RepositoryConfig {
            token: Some("push".into()),
            pull_request: Some(anodizer_core::config::PullRequestConfig {
                token: Some("pr".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            resolve_pr_token(&ctx, Some(&repo), None)
                .unwrap()
                .as_deref(),
            Some("pr")
        );
        assert_eq!(
            resolve_repo_token(&ctx, Some(&repo), None)
                .unwrap()
                .as_deref(),
            Some("push")
        );
    }

    /// A `repository.token` that renders to only whitespace falls through to
    /// the publisher variable, and a padded token is sent trimmed.
    #[test]
    fn a_whitespace_repository_token_falls_through_and_a_padded_one_is_trimmed() {
        let mut ctx = TestContextBuilder::new()
            .sealed_env()
            .env("KREW_INDEX_TOKEN", "krew")
            .build();
        ctx.template_vars_mut().set_env("BLANK_TOKEN", "   ");
        ctx.template_vars_mut()
            .set_env("PADDED_TOKEN", "  ghp_x\t ");
        let repo = |tok: &str| RepositoryConfig {
            token: Some(tok.into()),
            ..Default::default()
        };
        let resolve = |tok: &str| {
            resolve_repo_token(&ctx, Some(&repo(tok)), Some("KREW_INDEX_TOKEN")).unwrap()
        };
        assert_eq!(resolve("{{ .Env.BLANK_TOKEN }}").as_deref(), Some("krew"));
        assert_eq!(resolve("{{ .Env.PADDED_TOKEN }}").as_deref(), Some("ghp_x"));
    }
}
