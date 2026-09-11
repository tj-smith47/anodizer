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
///    re-derived, but ONLY for the crate whose family that tag belongs to. The
///    declared tag is run-wide while a release is per-crate, so a push of
///    `operator-v1.2.3` names the `operator` release and says nothing about
///    `app`'s.
/// 4. the crate's [`tag_family_template`](crate::config::CrateConfig::tag_family_template).
///
/// A `nightly.tag_name` is prefixed with the crate's own tag family in a
/// workspace that creates more than one (`operator-v` + `edge` →
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
    } else if let Some(tmpl) = release_tag_override {
        let source = "the release.tag override";
        let rendered = ctx
            .render_template(tmpl)
            .with_context(|| format!("release: render {source} for crate '{crate_name}'"))?;
        (rendered, source)
    } else if let Some(declared) = declared_tag_for_crate(ctx, crate_cfg) {
        (declared, "the declared current tag")
    } else {
        let source = "tag_template";
        let rendered = ctx
            .render_template(&crate_cfg.tag_family_template())
            .with_context(|| format!("release: render {source} for crate '{crate_name}'"))?;
        (rendered, source)
    };
    if rendered.is_empty() {
        anyhow::bail!(
            "release: {source} for crate '{crate_name}' rendered to an empty tag. \
             {EMPTY_RELEASE_TAG_HELP}"
        );
    }
    Ok(rendered)
}

/// What to tell the operator when a release tag resolves to nothing.
///
/// Shared verbatim by every surface that needs a tag — the release stage, the
/// `curl | sh` installer and cargo-binstall's `pkg_url` — so the same config
/// bug reads the same way wherever it surfaces first.
pub const EMPTY_RELEASE_TAG_HELP: &str = concat!(
    "A release cannot be created, downloaded or installed without a tag: the ",
    "GitHub / GitLab / Gitea Releases REST API rejects an empty `tag_name` with ",
    "a confusing 422 (`tag_name is too short`) that hides the real cause. Check ",
    "that `release.tag:` is not set to an empty string, and that the template it ",
    "or the crate's `tag_template` uses references a variable that is populated ",
    "on this run (`{{ Tag }}` is unset under `--snapshot` with no `tag_template` ",
    "fallback).",
);

/// The tag the operator named for this run, before any per-crate scoping.
///
/// `ANODIZER_CURRENT_TAG` (and a tag-push `GITHUB_REF_NAME`) states the tag
/// being released outright; anything else on `git_info` is a guess read off
/// the repository. Callers that need "the tag that was pushed" — the release
/// tag's declared rung, the `release.tag` divergence warning — read it here
/// rather than from the `Tag` template var, which every crate's
/// [`anchor_crate_tag`] rewrites.
pub fn declared_tag(ctx: &Context) -> Option<&str> {
    ctx.git_info
        .as_ref()
        .filter(|g| g.tag_source == crate::git::TagSource::Declared)
        .map(|g| g.tag.as_str())
        .filter(|t| !t.is_empty())
}

/// The tag the operator named for this run, when they named one AND it belongs
/// to this crate's tag family.
///
/// Re-deriving a declared tag from a template answers a question nobody asked
/// and can answer it differently — a repo whose tags carry a suffix the
/// template does not know about would have its release created on a tag that
/// is not the one pushed.
///
/// The family test is what keeps that from over-reaching: the declared tag is
/// one run-wide string, but a per-crate workspace releases several tracks in
/// one run, so a pushed `operator-v1.2.3` must not become the tag `app`'s
/// release is created on. Membership goes through the same sibling-aware
/// matcher the retention sweep and the previous-tag search use, so all three
/// agree on what "this crate's family" means — a bare `v` family does not
/// take a nested `vault-v1.0.0` away from the `vault` track.
fn declared_tag_for_crate(ctx: &Context, crate_cfg: &CrateConfig) -> Option<String> {
    let family = crate_cfg.tag_family_template();
    let siblings = ctx.config.sibling_tag_families_of(&family);
    declared_tag(ctx)
        .filter(|t| {
            crate::git::tag_in_family_excluding_siblings(
                t,
                &family,
                ctx.config.monorepo_tag_prefix(),
                &siblings,
            )
        })
        .map(str::to_string)
}

/// The tag TEMPLATE a crate's release is issued from: an explicit
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
/// release is, which is never the rolling tag a nightly run creates.
pub fn release_tag_template(crate_cfg: &CrateConfig) -> String {
    crate_cfg
        .release_tag_override()
        .map(str::to_string)
        .unwrap_or_else(|| crate_cfg.tag_family_template())
}

/// Put a literal `nightly.tag_name` inside this crate's tag family when the
/// workspace creates more than one, leaving it verbatim otherwise.
fn scope_to_tag_family(ctx: &Context, crate_cfg: &CrateConfig, rendered: String) -> String {
    if rendered.is_empty() || !ctx.config.mints_multiple_tag_families() {
        return rendered;
    }
    let prefix = crate::git::tag_family_prefix(
        &crate_cfg.tag_family_template(),
        ctx.config.monorepo_tag_prefix(),
    );
    match prefix {
        Some(p) => crate::git::compose_prefix(&p, &rendered),
        None => rendered,
    }
}

/// Re-anchor `Tag` and `PreviousTag` to one crate's own release tag.
///
/// A run-wide `Tag` is whichever family the version base came from, so in a
/// workspace creating several families every crate but that one would render
/// `{{ Tag }}` — release header, blob directory, announce body, compare link
/// — against a tag its release was never created on. Both variables move
/// together: a `PreviousTag` left behind from another family would bound a
/// changelog range that spans two tracks.
pub fn anchor_crate_tag(ctx: &mut Context, crate_cfg: &CrateConfig, tag: &str, log: &StageLogger) {
    let tag_template = crate_cfg.tag_family_template();
    let siblings = ctx.config.sibling_tag_families_of(&tag_template);
    let excluded = crate::git::excluded_sibling_prefixes(
        &tag_template,
        ctx.config.monorepo_tag_prefix(),
        &siblings,
    );
    if !excluded.is_empty() {
        log.verbose(&format!(
            "previous-tag search for family '{tag_template}' excludes sibling prefixes: {}",
            excluded.join(", ")
        ));
    }
    let previous = crate::git::find_previous_tag_in_family(
        tag,
        &tag_template,
        ctx.config.git.as_ref(),
        Some(ctx.template_vars()),
        ctx.config.monorepo_tag_prefix(),
        &siblings,
    );
    ctx.template_vars_mut().set("Tag", tag);
    match previous {
        Ok(Some(prev)) => ctx.template_vars_mut().set("PreviousTag", &prev),
        Ok(None) => {
            ctx.template_vars_mut().unset("PreviousTag");
        }
        Err(e) => {
            // A failed lookup must degrade to "no previous tag", never to the
            // run-wide value inherited from another family: that would bound a
            // changelog range spanning two tracks.
            ctx.template_vars_mut().unset("PreviousTag");
            log.verbose(&format!(
                "previous-tag lookup for crate '{}' failed: {e}",
                crate_cfg.name
            ));
        }
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

    /// The template resolver reads `release.tag:` off the crate config itself,
    /// so an installer or a `pkg_url` derived from it honours the override
    /// without every caller re-deriving where the override lives.
    #[test]
    fn release_tag_template_reads_the_crate_release_override() {
        let mut cfg = crate_cfg("app", "v{{ Version }}");
        assert_eq!(release_tag_template(&cfg), "v{{ Version }}");
        cfg.release = Some(crate::config::ReleaseConfig {
            tag: Some("release-{{ Version }}".to_string()),
            ..Default::default()
        });
        assert_eq!(release_tag_template(&cfg), "release-{{ Version }}");
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
    /// crate's own family that decides what this run creates.
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

    /// Build a context whose git info declares `tag`, over `crates`.
    fn declared_ctx(crates: Vec<CrateConfig>, tag: &str) -> Context {
        let config = Config {
            crates,
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        let mut info = crate::test_helpers::make_git_info(false, None);
        info.tag = tag.to_string();
        info.tag_source = crate::git::TagSource::Declared;
        ctx.git_info = Some(info);
        ctx
    }

    /// `release.tag` is the operator naming the tag for THIS crate; the
    /// declared tag is whatever ref the run started from. The narrower knob
    /// wins, or a tag-push run silently ignores the override entirely.
    #[test]
    fn release_tag_override_beats_a_declared_tag() {
        let cfg = crate_cfg("app", "v{{ Version }}");
        let ctx = declared_ctx(vec![cfg.clone()], "v1.0.0");
        assert_eq!(
            resolve_release_tag(&ctx, &cfg, Some("release-{{ Version }}")).unwrap(),
            "release-1.0.0",
        );
    }

    /// The empty-override bail must fire on a tag-push run too: a declared tag
    /// standing in for an empty `release.tag` hides the config bug instead of
    /// reporting it.
    #[test]
    fn empty_release_tag_bails_on_a_declared_tag_run() {
        let cfg = crate_cfg("app", "v{{ Version }}");
        let ctx = declared_ctx(vec![cfg.clone()], "v1.0.0");
        let err = resolve_release_tag(&ctx, &cfg, Some(""))
            .expect_err("an empty release.tag must bail even with a declared tag")
            .to_string();
        assert!(err.contains("release.tag"), "got: {err}");
        assert!(err.contains("app"), "got: {err}");
    }

    /// A declared tag is ONE run-wide string while a release is per crate. In
    /// a two-family workspace a pushed `operator-v1.2.3` names the operator
    /// release and says nothing about `app`'s, which must still come from its
    /// own family.
    #[test]
    fn declared_tag_of_another_family_is_not_used_for_this_crate() {
        let app = crate_cfg("app", "app-v{{ Version }}");
        let operator = crate_cfg("operator", "operator-v{{ Version }}");
        let ctx = declared_ctx(vec![app.clone(), operator.clone()], "operator-v1.2.3");
        assert_eq!(
            resolve_release_tag(&ctx, &operator, None).unwrap(),
            "operator-v1.2.3",
            "the declared tag belongs to the operator family",
        );
        assert_eq!(
            resolve_release_tag(&ctx, &app, None).unwrap(),
            "app-v1.0.0",
            "app must not be released under the operator track's tag",
        );
    }

    /// A bare `v` family starts every `vault-v…` tag too. The declared tag
    /// belongs to the narrowest configured family that claims it, so the `v`
    /// crate must fall through to its own template.
    #[test]
    fn declared_tag_of_a_nested_sibling_family_is_not_used_for_this_crate() {
        let app = crate_cfg("app", "v{{ Version }}");
        let vault = crate_cfg("vault", "vault-v{{ Version }}");
        let ctx = declared_ctx(vec![app.clone(), vault.clone()], "vault-v1.2.3");
        assert_eq!(
            resolve_release_tag(&ctx, &vault, None).unwrap(),
            "vault-v1.2.3",
            "the declared tag belongs to the vault family",
        );
        assert_eq!(
            resolve_release_tag(&ctx, &app, None).unwrap(),
            "v1.0.0",
            "a `v` family must not swallow a nested sibling's tag",
        );
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

    /// Both variables move together. When the lookup itself fails, the crate
    /// keeps its own `Tag` but a `PreviousTag` inherited from another family
    /// must go — leaving it would bound a changelog range across two tracks.
    #[test]
    #[serial_test::serial(cwd)]
    fn a_failed_previous_tag_lookup_drops_the_inherited_previous_tag() {
        let dir = tempfile::tempdir().unwrap();
        let _cwd = crate::test_helpers::CwdGuard::new(dir.path()).unwrap();
        let cfg = crate_cfg("operator", "operator-v{{ Version }}");
        let config = Config {
            crates: vec![cfg.clone()],
            // `smartsemver` lists the tags up front, so a directory that is no
            // repository surfaces as an error rather than "no previous tag".
            git: Some(crate::config::GitConfig {
                tag_sort: Some("smartsemver".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v0.10.0");
        ctx.template_vars_mut().set("PreviousTag", "v0.9.0");
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
        assert_eq!(ctx.template_vars().get("PreviousTag"), None);
    }
}
