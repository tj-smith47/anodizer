//! Release-asset download-URL derivation, shared by every consumer of
//! `metadata["url"]`.
//!
//! The download URL for a release asset is fully derivable from
//! `(provider, download_base, owner, repo, tag, asset name)` — all known
//! before any network call. The release stage stamps it after a live upload,
//! its dry-run stamps the identical derivation, and the emission-validate
//! pass seeds it ahead of the publisher renders (homebrew casks, winget,
//! scoop, …) so snapshot / dry-run pipelines validate the same URL a real
//! release would ship instead of failing on its absence.

use crate::config::{CrateConfig, ForceTokenKind, ReleaseConfig, ScmRepoConfig};
use crate::context::Context;
use crate::scm::ScmTokenType;
use anyhow::{Context as _, Result};

/// Seed the derived download URL onto every url-less artifact of one crate,
/// mirroring the release stage's derivation so a pre-release validation pass
/// (emission-validate) checks the SAME URL a real release would stamp instead
/// of failing on its absence.
///
/// Returns the URL prefix that was applied, or `None` when a real release
/// would leave the URLs absent too — no `release:` config, a truthy
/// `release.skip`, a `skip_upload` that suppresses uploads on a real release
/// (`auto` maps to `false` here: it skips only snapshots, and a snapshot
/// never runs the release stage at all — the question this pass answers is
/// "would a real release seed the URL?"), an unresolvable repo, or a tag
/// template that renders empty/erroneous. In each `None` case the downstream
/// publisher render surfaces its own actionable missing-url error, exactly as
/// the corresponding real run would.
pub fn seed_missing_download_urls_for_crate(
    ctx: &mut Context,
    crate_cfg: &CrateConfig,
) -> Result<Option<String>> {
    let Some(release_cfg) = crate_cfg.release.clone() else {
        return Ok(None);
    };
    if let Some(skip) = release_cfg.skip.as_ref()
        && skip.try_evaluates_to_true(|s| ctx.render_template(s))?
    {
        return Ok(None);
    }
    if let Some(su) = release_cfg.skip_upload.as_ref() {
        let rendered = if su.is_template() {
            ctx.render_template(su.as_str()).unwrap_or_default()
        } else {
            su.as_str().to_string()
        };
        if matches!(rendered.trim(), "true" | "1") {
            return Ok(None);
        }
    }

    // Same template-precedence the release stage's `resolve_release_tag`
    // applies: an explicit `release.tag:` override wins over `tag_template`.
    // A render error or empty result skips seeding rather than bailing — the
    // release stage owns the loud diagnostic for that config bug.
    let tag_tmpl = release_cfg
        .tag
        .as_deref()
        .unwrap_or(crate_cfg.tag_template.as_str());
    let tag = match ctx.render_template(tag_tmpl) {
        Ok(t) if !t.is_empty() => t,
        _ => return Ok(None),
    };

    let token_type = ctx.token_type;
    let Some(repo) = resolve_release_repo(&release_cfg, token_type, ctx)? else {
        return Ok(None);
    };
    let base = default_download_base(ctx);
    let prefix = release_download_url_prefix(token_type, &base, &repo.owner, &repo.name, &tag);
    seed_missing_artifact_download_urls(
        ctx,
        &crate_cfg.name,
        token_type,
        &base,
        &repo.owner,
        &repo.name,
        &tag,
    );
    Ok(Some(prefix))
}

/// The provider-shaped URL prefix every asset name is appended to:
/// `{base}/{owner}/{repo}/releases/download/{tag}` for GitHub/Gitea,
/// the `/-/releases/{tag}/downloads` form for GitLab (owner segment
/// omitted when empty — a top-level project with no namespace).
pub fn release_download_url_prefix(
    token_type: ScmTokenType,
    download_base: &str,
    owner: &str,
    repo: &str,
    tag: &str,
) -> String {
    let dl_base = download_base.trim_end_matches('/');
    let url_tag = crate::url::percent_encode_path_segment(tag);
    match token_type {
        ScmTokenType::GitLab => {
            if owner.is_empty() {
                format!("{dl_base}/{repo}/-/releases/{url_tag}/downloads")
            } else {
                format!("{dl_base}/{owner}/{repo}/-/releases/{url_tag}/downloads")
            }
        }
        ScmTokenType::GitHub | ScmTokenType::Gitea => {
            format!("{dl_base}/{owner}/{repo}/releases/download/{url_tag}")
        }
    }
}

/// Set `metadata["url"]` on every artifact for the given crate, constructing
/// the download URL from the SCM backend's download base, owner/repo, tag, and
/// artifact name. This lets publishers resolve download URLs without an
/// explicit `url_template`. Overwrites any prior value — the release stage's
/// post-upload call is authoritative.
pub fn populate_artifact_download_urls(
    ctx: &mut Context,
    crate_name: &str,
    token_type: ScmTokenType,
    download_base: &str,
    owner: &str,
    repo: &str,
    tag: &str,
) {
    set_artifact_download_urls(
        ctx,
        crate_name,
        token_type,
        download_base,
        owner,
        repo,
        tag,
        true,
    );
}

/// [`populate_artifact_download_urls`] that only fills artifacts still
/// MISSING a `url` — the emission-validate seeding path. Never clobbers a
/// value a release upload (or an earlier authoritative pass) already stamped.
pub fn seed_missing_artifact_download_urls(
    ctx: &mut Context,
    crate_name: &str,
    token_type: ScmTokenType,
    download_base: &str,
    owner: &str,
    repo: &str,
    tag: &str,
) {
    set_artifact_download_urls(
        ctx,
        crate_name,
        token_type,
        download_base,
        owner,
        repo,
        tag,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
fn set_artifact_download_urls(
    ctx: &mut Context,
    crate_name: &str,
    token_type: ScmTokenType,
    download_base: &str,
    owner: &str,
    repo: &str,
    tag: &str,
    overwrite: bool,
) {
    let url_prefix = release_download_url_prefix(token_type, download_base, owner, repo, tag);
    for artifact in ctx.artifacts.all_mut() {
        if artifact.crate_name == crate_name
            && !artifact.name.is_empty()
            && (overwrite || !artifact.metadata.contains_key("url"))
        {
            let encoded_name = crate::url::percent_encode_path_segment(&artifact.name);
            artifact
                .metadata
                .insert("url".to_string(), format!("{url_prefix}/{encoded_name}"));
        }
    }
}

/// The provider's public download base when no live backend response is
/// available (dry-run, snapshot, offline seeding): the configured
/// `<provider>_urls.download` override, else the hosted default.
pub fn default_download_base(ctx: &Context) -> String {
    match ctx.token_type {
        ScmTokenType::GitHub => ctx
            .config
            .github_urls
            .as_ref()
            .and_then(|u| u.download.clone())
            .unwrap_or_else(|| "https://github.com".to_string()),
        ScmTokenType::GitLab => ctx
            .config
            .gitlab_urls
            .as_ref()
            .and_then(|u| u.download.clone())
            .unwrap_or_else(|| "https://gitlab.com".to_string()),
        ScmTokenType::Gitea => ctx
            .config
            .gitea_urls
            .as_ref()
            .and_then(|u| u.download.clone())
            .unwrap_or_else(|| {
                ctx.config
                    .gitea_urls
                    .as_ref()
                    .and_then(|u| u.api.as_deref())
                    .map(|api| {
                        api.trim_end_matches('/')
                            .trim_end_matches("/api/v1")
                            .to_string()
                    })
                    .unwrap_or_else(|| "https://gitea.com".to_string())
            }),
    }
}

/// Pick the `ScmRepoConfig` for the active publish target and template-render
/// its `owner` and `name` fields.
///
/// Resolution order:
/// 1. Explicit `release.provider:`.
/// 2. Active SCM token type with provider-side fallback (the historical
///    behaviour — preserved so existing configs don't change shape).
///
/// Returns `Ok(None)` when no matching block is configured.
pub fn resolve_release_repo(
    release_cfg: &ReleaseConfig,
    token_type: ScmTokenType,
    ctx: &Context,
) -> Result<Option<ScmRepoConfig>> {
    // Explicit `release.provider:` wins over token-type inference. This
    // is the cross-platform publishing seam: a project hosted on GitLab
    // (so `GITLAB_TOKEN` is the active token) can declare
    // `provider: github` to redirect publish output to GitHub.
    let raw = match release_cfg.provider {
        Some(ForceTokenKind::GitHub) => release_cfg.github.as_ref(),
        Some(ForceTokenKind::GitLab) => release_cfg.gitlab.as_ref(),
        Some(ForceTokenKind::Gitea) => release_cfg.gitea.as_ref(),
        None => match token_type {
            ScmTokenType::GitLab => release_cfg.gitlab.as_ref().or(release_cfg.github.as_ref()),
            ScmTokenType::Gitea => release_cfg.gitea.as_ref().or(release_cfg.github.as_ref()),
            ScmTokenType::GitHub => release_cfg.github.as_ref(),
        },
    };
    let Some(repo) = raw else {
        return Ok(None);
    };
    let owner = ctx
        .render_template(&repo.owner)
        .with_context(|| format!("release: render repo.owner '{}'", repo.owner))?;
    let name = ctx
        .render_template(&repo.name)
        .with_context(|| format!("release: render repo.name '{}'", repo.name))?;
    Ok(Some(ScmRepoConfig {
        owner,
        name,
        token: repo.token.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::{Artifact, ArtifactKind};
    use crate::config::{Config, CrateConfig, StringOrBool};
    use crate::context::ContextOptions;
    use std::collections::HashMap;

    fn archive(crate_name: &str, name: &str, target: &str) -> Artifact {
        Artifact {
            kind: ArtifactKind::Archive,
            path: format!("dist/{name}").into(),
            name: name.to_string(),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: HashMap::new(),
            size: None,
        }
    }

    fn crate_cfg_with_release() -> CrateConfig {
        CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "octocat".to_string(),
                    name: "hello".to_string(),
                    token: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn ctx_with(crate_cfg: &CrateConfig, artifacts: Vec<Artifact>) -> Context {
        let config = Config {
            crates: vec![crate_cfg.clone()],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        for a in artifacts {
            ctx.artifacts.add(a);
        }
        ctx
    }

    /// The seeding path derives the exact GitHub download URL a real release
    /// stamps — repo from `release.github`, tag from `tag_template`.
    #[test]
    fn seed_fills_missing_url_with_release_derivation() {
        let cfg = crate_cfg_with_release();
        let mut ctx = ctx_with(
            &cfg,
            vec![archive(
                "myapp",
                "myapp-1.0.0-aarch64-apple-darwin.tar.gz",
                "aarch64-apple-darwin",
            )],
        );
        let prefix = seed_missing_download_urls_for_crate(&mut ctx, &cfg)
            .unwrap()
            .expect("derivable config must seed");
        assert_eq!(
            prefix,
            "https://github.com/octocat/hello/releases/download/v1.0.0"
        );
        assert_eq!(
            ctx.artifacts.all()[0]
                .metadata
                .get("url")
                .map(String::as_str),
            Some(
                "https://github.com/octocat/hello/releases/download/v1.0.0/myapp-1.0.0-aarch64-apple-darwin.tar.gz"
            )
        );
    }

    /// A url stamped by an earlier authoritative pass (a release upload) must
    /// survive the seeding — fill-missing only, never overwrite.
    #[test]
    fn seed_never_overwrites_existing_url() {
        let cfg = crate_cfg_with_release();
        let mut art = archive("myapp", "myapp.tar.gz", "aarch64-apple-darwin");
        art.metadata.insert(
            "url".to_string(),
            "https://real.example/x.tar.gz".to_string(),
        );
        let mut ctx = ctx_with(&cfg, vec![art]);
        seed_missing_download_urls_for_crate(&mut ctx, &cfg).unwrap();
        assert_eq!(
            ctx.artifacts.all()[0]
                .metadata
                .get("url")
                .map(String::as_str),
            Some("https://real.example/x.tar.gz")
        );
    }

    /// A crate whose real release is skipped gets no seeded URL — the real run
    /// would leave it absent too, and the validators must see that truth.
    #[test]
    fn seed_skips_when_release_skipped_or_absent() {
        // release.skip: true
        let mut skipped = crate_cfg_with_release();
        skipped.release.as_mut().unwrap().skip = Some(StringOrBool::Bool(true));
        let mut ctx = ctx_with(&skipped, vec![archive("myapp", "a.tar.gz", "x")]);
        assert!(
            seed_missing_download_urls_for_crate(&mut ctx, &skipped)
                .unwrap()
                .is_none()
        );
        assert!(!ctx.artifacts.all()[0].metadata.contains_key("url"));

        // no release: block at all
        let mut no_release = crate_cfg_with_release();
        no_release.release = None;
        let mut ctx = ctx_with(&no_release, vec![archive("myapp", "a.tar.gz", "x")]);
        assert!(
            seed_missing_download_urls_for_crate(&mut ctx, &no_release)
                .unwrap()
                .is_none()
        );

        // skip_upload: true (a real release would never stamp urls)
        let mut no_upload = crate_cfg_with_release();
        no_upload.release.as_mut().unwrap().skip_upload = Some(StringOrBool::Bool(true));
        let mut ctx = ctx_with(&no_upload, vec![archive("myapp", "a.tar.gz", "x")]);
        assert!(
            seed_missing_download_urls_for_crate(&mut ctx, &no_upload)
                .unwrap()
                .is_none()
        );
    }

    /// `release.tag:` override wins over `tag_template`, mirroring
    /// `resolve_release_tag`'s precedence in the live release stage.
    #[test]
    fn seed_honors_release_tag_override() {
        let mut cfg = crate_cfg_with_release();
        cfg.release.as_mut().unwrap().tag = Some("app-v{{ .Version }}".to_string());
        let mut ctx = ctx_with(&cfg, vec![archive("myapp", "a.tar.gz", "x")]);
        let prefix = seed_missing_download_urls_for_crate(&mut ctx, &cfg)
            .unwrap()
            .unwrap();
        assert_eq!(
            prefix,
            "https://github.com/octocat/hello/releases/download/app-v1.0.0"
        );
    }

    /// Seeding scopes strictly by crate: a sibling crate's artifacts are
    /// untouched (per-crate config mode).
    #[test]
    fn seed_scopes_to_the_named_crate_only() {
        let cfg = crate_cfg_with_release();
        let mut ctx = ctx_with(
            &cfg,
            vec![
                archive("myapp", "a.tar.gz", "x"),
                archive("other", "b.tar.gz", "x"),
            ],
        );
        seed_missing_download_urls_for_crate(&mut ctx, &cfg).unwrap();
        let arts = ctx.artifacts.all();
        assert!(
            arts.iter()
                .any(|a| a.crate_name == "myapp" && a.metadata.contains_key("url"))
        );
        assert!(
            arts.iter()
                .all(|a| a.crate_name != "other" || !a.metadata.contains_key("url"))
        );
    }

    /// GitLab with an empty owner omits the namespace segment — same shape the
    /// release stage derives.
    #[test]
    fn prefix_gitlab_empty_owner_omits_namespace() {
        assert_eq!(
            release_download_url_prefix(
                ScmTokenType::GitLab,
                "https://gitlab.com/",
                "",
                "proj",
                "v1"
            ),
            "https://gitlab.com/proj/-/releases/v1/downloads"
        );
        assert_eq!(
            release_download_url_prefix(
                ScmTokenType::GitLab,
                "https://gitlab.com",
                "grp",
                "proj",
                "v1"
            ),
            "https://gitlab.com/grp/proj/-/releases/v1/downloads"
        );
    }
}
