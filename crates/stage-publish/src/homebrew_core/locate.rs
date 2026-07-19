//! Formula-file location: resolves an explicit `path:` override or falls back
//! to the sharded/flat formula-path layout via the GitHub contents API.

use anodizer_core::config::HomebrewCoreConfig;
use anodizer_core::context::Context;
use anyhow::{Context as _, Result};

use super::api::{GithubApi, RepoFile};
use super::formula::{flat_formula_path, sharded_formula_path};

pub(super) fn locate_formula(
    ctx: &Context,
    cfg: &HomebrewCoreConfig,
    api: &GithubApi,
    owner: &str,
    repo: &str,
    branch: &str,
    formula: &str,
) -> Result<Option<RepoFile>> {
    if let Some(raw) = cfg.path.as_deref().filter(|p| !p.is_empty()) {
        let path = ctx
            .render_template(raw)
            .context("homebrew-core: render path template")?;
        return api.get_file(owner, repo, &path, branch);
    }
    if let Some(f) = api.get_file(owner, repo, &sharded_formula_path(formula), branch)? {
        return Ok(Some(f));
    }
    api.get_file(owner, repo, &flat_formula_path(formula), branch)
}
