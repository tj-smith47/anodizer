//! The one tag a crate's release is created, captured and verified under.
//!
//! Every surface that composes a release URL, an installer tag, a publish
//! target or the release itself resolves the tag HERE. A second derivation
//! anywhere else re-opens the class of bug where the release is created on one
//! tag and named, captured or downloaded from another.

use anyhow::{Context as _, Result};

use crate::config::CrateConfig;
use crate::context::Context;
use crate::log::StageLogger;

/// Resolve the tag a crate's release is created, captured and verified under.
///
/// One answer for all three: `release_one_crate` creates on it,
/// `collect_release_targets` looks the release id up by it, and
/// `fetch_published_assets` probes it. Precedence, narrowest first:
///
/// 1. `nightly.tag_name` — nightly runs only, so the narrower knob wins.
/// 2. `release.tag` — the Pro `release.tag` override, on every run.
/// 3. the tag the operator declared for this run (`ANODIZER_CURRENT_TAG`, or a
///    tag-push `GITHUB_REF_NAME`) — already the exact tag, so it is taken, not
///    re-derived.
/// 4. the crate's [`tag_family_template`](crate::config::CrateConfig::tag_family_template).
///
/// A `nightly.tag_name` is prefixed with the crate's own tag family in a
/// workspace that mints more than one (`operator-v` + `edge` →
/// `operator-vedge`): one tag cannot carry three tracks' releases, and a tag
/// inside the family is also what scopes the retention sweep to this track.
/// The prefix comes from the same `tag_family_scope` the retention matcher
/// applies, so a `monorepo.tag_prefix` namespace is honoured identically on
/// both sides. A single-family workspace takes the value verbatim.
///
/// Bails when the rendered tag is empty: the GitHub / GitLab / Gitea Releases
/// REST APIs all require a non-empty `tag_name`, and silently POSTing
/// `tag_name: ""` returns a confusing 422 (`tag_name is too short`) that
/// hides the real cause (template rendered to empty because a referenced
/// variable was missing on the snapshot path).
pub fn resolve_release_tag(
    ctx: &Context,
    crate_cfg: &CrateConfig,
    release_tag_override: Option<&str>,
) -> Result<String> {
    let crate_name = crate_cfg.name.as_str();
    let nightly_tag_name = if ctx.is_nightly() {
        ctx.config
            .nightly
            .as_ref()
            .and_then(|n| n.tag_name.as_deref())
            .filter(|s| !s.trim().is_empty())
    } else {
        None
    };

    let (rendered, source) = if let Some(tmpl) = nightly_tag_name {
        let rendered = ctx
            .render_template(tmpl)
            .with_context(|| format!("release: render nightly.tag_name for crate '{crate_name}'"))?
            .trim()
            .to_string();
        (
            scope_to_tag_family(ctx, crate_cfg, rendered),
            "nightly.tag_name",
        )
    } else if let Some(declared) = declared_tag(ctx) {
        (declared, "the declared current tag")
    } else {
        let source = if release_tag_override.is_some() {
            "the release.tag override"
        } else {
            "tag_template"
        };
        let tmpl = release_tag_template(crate_cfg, release_tag_override);
        let rendered = ctx
            .render_template(&tmpl)
            .with_context(|| format!("release: render {source} for crate '{crate_name}'"))?;
        (rendered, source)
    };
    if rendered.is_empty() {
        anyhow::bail!(
            "release: {} for crate '{}' rendered to an empty tag. The GitHub / \
             GitLab / Gitea Releases REST API requires a non-empty `tag_name`; \
             posting an empty value returns a confusing 422 (`tag_name is too \
             short`) that hides the real cause. Verify the template references \
             a variable that is populated on this run (e.g. `{{{{ Tag }}}}` is \
             unset during `--snapshot` without a `tag_template` fallback) or \
             set an explicit `release.tag:` override.",
            source,
            crate_name
        );
    }
    Ok(rendered)
}

/// The tag the operator named for this run, when they named one.
///
/// `ANODIZER_CURRENT_TAG` (and the tag-push `GITHUB_REF_NAME`) states the tag
/// being released outright. Re-deriving it from a template answers a question
/// nobody asked and can answer it differently — a repo whose tags carry a
/// suffix the template does not know about would have its release created on
/// a tag that is not the one pushed.
fn declared_tag(ctx: &Context) -> Option<String> {
    ctx.git_info
        .as_ref()
        .filter(|g| g.tag_source == crate::git::TagSource::Declared)
        .map(|g| g.tag.clone())
        .filter(|t| !t.is_empty())
}

/// The tag TEMPLATE a crate's release is minted from: an explicit
/// `release.tag` override, else the crate's own tag family.
///
/// An override that is present but empty is returned as-is: `release.tag: ""`
/// is a config bug, and every consumer of this template fails loudly on an
/// empty tag. Falling back to the crate's family instead would paper over it
/// and ship a release under a tag the operator did not ask for.
///
/// This is the version-PARAMETERISED half of [`resolve_release_tag`], for the
/// surfaces that emit a template rather than a tag — the `curl | sh` installer
/// and cargo-binstall's `pkg_url` both resolve a version at install time and
/// reconstruct the tag from it. They deliberately stop short of the
/// `nightly.tag_name` rung: both point at whatever the project's newest STABLE
/// release is, which is never the rolling tag a nightly run mints.
pub fn release_tag_template(crate_cfg: &CrateConfig, release_tag_override: Option<&str>) -> String {
    release_tag_override
        .map(str::to_string)
        .unwrap_or_else(|| crate_cfg.tag_family_template())
}

/// Put a literal `nightly.tag_name` inside this crate's tag family when the
/// workspace mints more than one, leaving it verbatim otherwise.
fn scope_to_tag_family(ctx: &Context, crate_cfg: &CrateConfig, rendered: String) -> String {
    if rendered.is_empty() || !ctx.config.mints_multiple_tag_families() {
        return rendered;
    }
    let prefix = crate::git::tag_family_prefix(
        &crate_cfg.tag_family_template(),
        ctx.config.monorepo_tag_prefix(),
    );
    match prefix {
        Some(p) if !p.is_empty() && !rendered.starts_with(&p) => format!("{p}{rendered}"),
        _ => rendered,
    }
}

/// Re-anchor `Tag` and `PreviousTag` to one crate's own release tag.
///
/// A run-wide `Tag` is whichever family the version base came from, so in a
/// workspace minting several families every crate but that one would render
/// `{{ Tag }}` — release header, blob directory, announce body, compare link
/// — against a tag its release was never created on. Both variables move
/// together: a `PreviousTag` left behind from another family would bound a
/// changelog range that spans two tracks.
pub fn anchor_crate_tag(ctx: &mut Context, crate_cfg: &CrateConfig, tag: &str, log: &StageLogger) {
    let tag_template = crate_cfg.tag_family_template();
    let previous = crate::git::find_previous_tag_in_family(
        tag,
        &tag_template,
        ctx.config.git.as_ref(),
        Some(ctx.template_vars()),
        ctx.config.monorepo_tag_prefix(),
    );
    ctx.template_vars_mut().set("Tag", tag);
    match previous {
        Ok(Some(prev)) => ctx.template_vars_mut().set("PreviousTag", &prev),
        Ok(None) => {
            ctx.template_vars_mut().unset("PreviousTag");
        }
        Err(e) => log.verbose(&format!(
            "previous-tag lookup for crate '{}' failed: {e}",
            crate_cfg.name
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::context::{Context, ContextOptions};

    fn crate_cfg(name: &str, tmpl: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: Some(tmpl.to_string()),
            ..Default::default()
        }
    }

    /// An operator-declared tag IS the tag being released. Re-rendering the
    /// template would answer a question nobody asked, and can answer it with
    /// a different string than the ref that was pushed.
    #[test]
    fn declared_tag_is_not_rederived() {
        let cfg = crate_cfg("app", "v{{ Version }}");
        let config = Config {
            crates: vec![cfg.clone()],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        let mut info = crate::test_helpers::make_git_info(false, None);
        info.tag = "v1.0.0+build.7".to_string();
        info.tag_source = crate::git::TagSource::Declared;
        ctx.git_info = Some(info);
        assert_eq!(
            resolve_release_tag(&ctx, &cfg, None).unwrap(),
            "v1.0.0+build.7",
        );
    }

    /// An INFERRED tag is a guess the template must still win over: it is the
    /// crate's own family that decides what this run mints.
    #[test]
    fn an_inferred_tag_still_goes_through_the_template() {
        let cfg = crate_cfg("app", "v{{ Version }}");
        let config = Config {
            crates: vec![cfg.clone()],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        let mut info = crate::test_helpers::make_git_info(false, None);
        info.tag = "v0.9.0".to_string();
        ctx.git_info = Some(info);
        assert_eq!(resolve_release_tag(&ctx, &cfg, None).unwrap(), "v1.0.0");
    }

    /// The whole point of the helper: after it runs, `Tag` names the tag the
    /// caller resolved, whatever it held before.
    #[test]
    fn anchoring_replaces_a_foreign_family_tag() {
        let cfg = crate_cfg("operator", "operator-v{{ Version }}");
        let config = Config {
            crates: vec![cfg.clone()],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v0.10.0");
        anchor_crate_tag(
            &mut ctx,
            &cfg,
            "operator-v2.4.0",
            crate::test_helpers::test_logger(),
        );
        assert_eq!(
            ctx.template_vars().get("Tag").map(String::as_str),
            Some("operator-v2.4.0"),
        );
    }
}
