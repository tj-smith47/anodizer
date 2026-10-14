//! Bump-input resolution: the token ladder, formula name, formula
//! repository, download URL, commit identity, bump branch, and commit
//! message.
//!
//! Every function here derives one bump input from config plus context —
//! no network calls, no orchestration — so each is unit-testable without
//! standing up a GitHub API.

use anodizer_core::config::HomebrewCoreConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};

/// The publisher's own token variables, read in this order: the dedicated
/// `HOMEBREW_CORE_GITHUB_TOKEN`, then `COMMITTER_TOKEN` (the name
/// mislav/bump-homebrew-formula-action reads, so a project moving from that
/// action keeps its existing secret).
pub(crate) const TOKEN_ENV_VARS: [&str; 2] = ["HOMEBREW_CORE_GITHUB_TOKEN", "COMMITTER_TOKEN"];

/// The default formula repository when `repository:` is unset.
const CORE_OWNER: &str = "Homebrew";
const CORE_REPO: &str = "homebrew-core";

/// A resolved bump token plus the env var that supplied it.
pub(crate) struct ResolvedToken {
    /// The token value used to authenticate the bump.
    pub token: String,
    /// The env var that supplied the token, recorded in the target snapshot
    /// so a rollback that finds no token can name it in its message. The
    /// rollback resolves the whole order again. `None` when the token came
    /// from `repository.token` or the run token.
    pub env_var: Option<String>,
}

/// The token sources [`resolve_token`] reads, in its order, for a message
/// that says what to set when none of them holds a token.
pub(crate) fn token_order_hint() -> String {
    format!(
        "`homebrew_cores[].repository.token`, ${}, ${}, the run's GitHub token \
         (`--token`), then ${}",
        TOKEN_ENV_VARS[0],
        TOKEN_ENV_VARS[1],
        anodizer_core::git::GITHUB_TOKEN_ENV_LADDER.join(" / $"),
    )
}

/// Resolve the bump token in the push order every publisher shares
/// ([`crate::util::resolve_repo_token`]): `repository.token`, then
/// [`TOKEN_ENV_VARS`], then the run token on a GitHub run, then
/// `ANODIZER_GITHUB_TOKEN` / `GITHUB_TOKEN` / `GH_TOKEN`. Each value is
/// trimmed and a blank one falls through. `Ok(None)` when nothing resolves;
/// `Err` only when a configured `repository.token` template fails to render.
pub(crate) fn resolve_token(
    ctx: &Context,
    cfg: &HomebrewCoreConfig,
) -> Result<Option<ResolvedToken>> {
    let configured =
        crate::util::configured_push_token(cfg.repository.as_ref(), &|s| ctx.render_template(s))?;
    if let Some(token) = configured {
        return Ok(Some(ResolvedToken {
            token,
            env_var: None,
        }));
    }
    Ok(crate::util::resolve_env_token_tracked(ctx, &TOKEN_ENV_VARS)
        .map(|(token, env_var)| ResolvedToken { token, env_var }))
}

/// Resolve the token the bump pull request opens with: `pull_request.token`,
/// else `repository.token` ([`crate::util::configured_pr_token`]), else the
/// bump token ([`resolve_token`]). The publish opens the pull request with
/// it, and the rollback closes it with the same one.
pub(crate) fn resolve_pr_token(ctx: &Context, cfg: &HomebrewCoreConfig) -> Result<Option<String>> {
    match crate::util::configured_pr_token(cfg.repository.as_ref(), &|s| ctx.render_template(s))? {
        Some(token) => Ok(Some(token)),
        None => Ok(resolve_token(ctx, cfg)?.map(|t| t.token)),
    }
}

/// Resolve the formula name: `cfg.name` (templated), else the first
/// `ids:`-scoped crate name, else the primary crate name, else the project
/// name.
pub(crate) fn resolve_formula_name(ctx: &Context, cfg: &HomebrewCoreConfig) -> Result<String> {
    if let Some(raw) = cfg.name.as_deref().filter(|n| !n.is_empty()) {
        return ctx
            .render_template(raw)
            .context("homebrew-core: render name template");
    }
    if let Some(first) = cfg.ids.as_ref().and_then(|ids| ids.first()) {
        return Ok(first.clone());
    }
    Ok(ctx
        .config
        .primary_crate_name()
        .map(str::to_string)
        .unwrap_or_else(|| ctx.config.project_name.clone()))
}

/// Resolve the formula repository `(owner, name)` — the configured
/// `repository:` when both halves are set, else `Homebrew/homebrew-core`.
pub(crate) fn resolve_upstream(cfg: &HomebrewCoreConfig) -> (String, String) {
    crate::util::resolve_repo_owner_name(cfg.repository.as_ref())
        .unwrap_or_else(|| (CORE_OWNER.to_string(), CORE_REPO.to_string()))
}

/// True when the bump targets `Homebrew/homebrew-core` itself, which never
/// accepts direct pushes or same-repo bot branches — always fork + PR.
pub(super) fn is_homebrew_core(owner: &str, repo: &str) -> bool {
    owner.eq_ignore_ascii_case(CORE_OWNER) && repo.eq_ignore_ascii_case(CORE_REPO)
}

/// Derive the source-repo `(owner, repo)` for the default tarball URL: the
/// `ids:`-scoped (else primary) crate's `release.github`, then the top-level
/// `release.github`, then the origin remote — the latter two via the
/// canonical [`resolve_github_slug`](anodizer_core::git::resolve_github_slug)
/// (config override → remote, applied once),
/// so the repo identity is never re-parsed ad hoc.
fn source_repo_coords(ctx: &Context, cfg: &HomebrewCoreConfig) -> Option<(String, String)> {
    let universe = ctx.config.crate_universe();
    let scoped = cfg
        .ids
        .as_ref()
        .and_then(|ids| ids.first())
        .and_then(|id| universe.iter().find(|c| &c.name == id))
        .or_else(|| {
            ctx.config
                .primary_crate_name()
                .and_then(|n| universe.iter().find(|c| c.name == n))
        });
    let gh = scoped
        .and_then(|c| c.release.as_ref())
        .and_then(|r| r.github.as_ref())
        .or_else(|| ctx.config.release.as_ref().and_then(|r| r.github.as_ref()));
    // A configured `release.github` is the slug override; absent one, the
    // resolver derives once from the origin remote.
    let owner = gh.and_then(|g| ctx.render_template(&g.owner).ok());
    let name = gh.and_then(|g| ctx.render_template(&g.name).ok());
    anodizer_core::git::resolve_github_slug(owner.as_deref(), name.as_deref())
        .ok()
        .map(|s| (s.owner().to_string(), s.name().to_string()))
}

/// Resolve the templated download URL, defaulting to the GitHub source
/// tarball for the release tag.
pub(crate) fn resolve_download_url(ctx: &Context, cfg: &HomebrewCoreConfig) -> Result<String> {
    if let Some(raw) = cfg.download_url.as_deref().filter(|u| !u.is_empty()) {
        return ctx
            .render_template(raw)
            .context("homebrew-core: render download_url template");
    }
    let Some((owner, repo)) = source_repo_coords(ctx, cfg) else {
        bail!(
            "homebrew-core: cannot derive the default download URL — set \
             `download_url:`, a `release.github` repo, or run inside a git \
             checkout with a github.com remote"
        );
    };
    let tag = ctx
        .template_vars()
        .get("Tag")
        .cloned()
        .unwrap_or_else(|| format!("v{}", ctx.version()));
    Ok(format!(
        "https://github.com/{}/{}/archive/refs/tags/{}.tar.gz",
        owner, repo, tag
    ))
}

/// Resolve the contents-API commit identity from `commit_author`.
///
/// Returns `None` — omit `author`/`committer` from the PUT so GitHub
/// attributes the commit to the token's own account — when no `commit_author`
/// is configured, or when `use_github_app_token` is set (the App-token
/// account, the canonical EasyCLA/DCO identity for homebrew-core, must author
/// the commit). Otherwise returns the resolved `(name, email)` (config →
/// local git identity → the anodizer default), reusing the same
/// `resolve_commit_opts` resolution the tap/winget/krew publishers apply.
pub(crate) fn resolve_commit_identity(
    ctx: &Context,
    cfg: &HomebrewCoreConfig,
    log: &StageLogger,
) -> Result<Option<(String, String)>> {
    let Some(ca) = cfg.commit_author.as_ref() else {
        return Ok(None);
    };
    let opts = crate::util::resolve_commit_opts(ctx, Some(ca), log)?;
    if opts.use_github_app_token {
        return Ok(None);
    }
    Ok(opts.author_name.zip(opts.author_email))
}

/// The bump branch name for one formula + version.
pub(crate) fn bump_branch(formula: &str, version: &str) -> String {
    format!("bump-{}-{}", formula, version)
}

/// The default commit message / PR title: `<formula> <version>` — the form
/// homebrew-core's CI expects for version bumps.
pub(crate) fn resolve_commit_message(
    ctx: &Context,
    cfg: &HomebrewCoreConfig,
    formula: &str,
    version: &str,
) -> Result<String> {
    match cfg.commit_msg_template.as_deref().filter(|t| !t.is_empty()) {
        Some(raw) => ctx
            .render_template(raw)
            .context("homebrew-core: render commit_msg_template"),
        None => Ok(format!("{} {}", formula, version)),
    }
}
