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

/// Env var fallback ladder for the bump token: the dedicated
/// `HOMEBREW_CORE_GITHUB_TOKEN`, then `COMMITTER_TOKEN` (the name
/// mislav/bump-homebrew-formula-action consumes, so a project migrating from
/// that action keeps its existing secret), then the standard GitHub ladder
/// (`ANODIZER_GITHUB_TOKEN` / `GITHUB_TOKEN`).
pub(crate) const TOKEN_ENV_VARS: [&str; 2] = ["HOMEBREW_CORE_GITHUB_TOKEN", "COMMITTER_TOKEN"];

/// The default formula repository when `repository:` is unset.
const CORE_OWNER: &str = "Homebrew";
const CORE_REPO: &str = "homebrew-core";

/// A resolved bump token plus the env var that supplied it.
pub(crate) struct ResolvedToken {
    /// The token value used to authenticate the bump.
    pub token: String,
    /// The env var the token came from, threaded into the target snapshot so
    /// `rollback` re-resolves through the SAME var (the H15 fix — a
    /// `COMMITTER_TOKEN`-sourced token must not record `HOMEBREW_CORE_GITHUB_TOKEN`).
    /// `None` when the token came from a templated `repository.token` (no
    /// single env var to record; rollback falls back to the GitHub ladder).
    pub env_var: Option<String>,
}

/// The full bump-token ladder: the dedicated `HOMEBREW_CORE_GITHUB_TOKEN` /
/// `COMMITTER_TOKEN`, then the standard GitHub ladder (`ANODIZER_GITHUB_TOKEN`
/// / `GITHUB_TOKEN` / `GH_TOKEN`). A `repository.token` template still wins
/// ahead of it.
fn token_env_ladder() -> Vec<&'static str> {
    TOKEN_ENV_VARS
        .iter()
        .copied()
        .chain(anodizer_core::git::GITHUB_TOKEN_ENV_LADDER.iter().copied())
        .collect()
}

/// Resolve the bump token: `repository.token` (templated) wins, then the
/// [`token_env_ladder`]. `Ok(None)` when nothing resolves; `Err` only when a
/// configured `repository.token` template fails to render. Empty values are
/// filtered at every rung by the shared helper.
pub(crate) fn resolve_token(
    ctx: &Context,
    cfg: &HomebrewCoreConfig,
) -> Result<Option<ResolvedToken>> {
    let configured = cfg.repository.as_ref().and_then(|r| r.token.as_deref());
    let (token, env_var) = crate::publisher_helpers::resolve_token_with_ladder_tracked(
        ctx,
        configured,
        "homebrew-core: render token template",
        &token_env_ladder(),
    )?;
    if token.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ResolvedToken { token, env_var }))
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
/// canonical [`resolve_github_slug`] (config override → remote, applied once),
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
