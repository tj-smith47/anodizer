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
/// 3. the crate's [`tag_family_template`](crate::config::CrateConfig::tag_family_template).
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
    } else if let Some(override_tmpl) = release_tag_override {
        let rendered = ctx.render_template(override_tmpl).with_context(|| {
            format!("release: render release.tag override for crate '{crate_name}'")
        })?;
        (rendered, "release.tag")
    } else {
        let tmpl = crate_cfg.tag_family_template();
        let rendered = ctx
            .render_template(&tmpl)
            .with_context(|| format!("release: render tag_template for crate '{crate_name}'"))?;
        (rendered, "tag_template")
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
