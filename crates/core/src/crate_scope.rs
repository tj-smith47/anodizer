//! Per-crate template-variable scoping shared by the build stage's source
//! mutations and the snapshot emission validator.
//!
//! A multi-crate invocation (`release --all`, or several independent-version
//! tags at HEAD) must render each crate's `version_sync` / binstall
//! `pkg-url`/`overrides` / nix derivation against THAT crate's own version and
//! name — not the first crate's. [`Context::populate_git_vars`] derives the
//! global `Version`/`Tag`/`RawVersion`/`ProjectName` from a single `GitInfo`
//! for the first crate, so without per-crate scoping every sibling inherits
//! the first crate's vars.
//!
//! This module centralizes the override derivation, tag resolution, and the
//! apply/restore RAII-style scope so both the real-release mutation path and
//! the snapshot validation path agree on exactly how a crate's vars are
//! resolved — they can never drift.

use std::collections::HashSet;

use anyhow::{Context as _, Result};

use crate::config::CrateConfig;
use crate::context::Context;

/// Template variables anodize re-scopes per crate.
const PER_CRATE_SCOPED_VARS: &[&str] = &["Version", "RawVersion", "Tag", "ProjectName", "Name"];

/// Per-crate template-variable overrides derived the same way
/// [`Context::populate_git_vars`] derives the global ones, but scoped to a
/// single crate's tag and name.
///
/// `tag` is the crate's own git tag (monorepo prefix already stripped for the
/// base `Tag`/`Version` vars); `name` is the crate's package name, used for
/// `ProjectName`/`Name` so binstall `pkg-url` templates referencing
/// `{{ .ProjectName }}` resolve per crate.
///
/// Returns an error when the tag is not parseable as semver: a per-crate
/// emission MUST get its own version, and silently falling back to the first
/// crate's vars would stamp the wrong version/URL. `Version`/`RawVersion` are
/// derived from the shared [`crate::git::SemVer`] helpers so the build stage,
/// the validator, and `populate_git_vars` never drift.
pub fn crate_template_overrides(name: &str, tag: &str) -> Result<Vec<(&'static str, String)>> {
    let semver = crate::git::parse_semver_tag(tag).with_context(|| {
        format!("crate '{name}': release tag '{tag}' is not a parseable semver version")
    })?;
    Ok(vec![
        ("RawVersion", semver.raw_version_string()),
        ("Version", semver.version_string()),
        ("Tag", tag.to_string()),
        ("ProjectName", name.to_string()),
        ("Name", name.to_string()),
    ])
}

/// Resolve a crate's own latest matching tag from git, monorepo prefix
/// stripped. Returns `None` when no tag matches; callers treat that as a
/// fail-loud error for an emission-enabled crate (never a silent fall-back to
/// the first crate's vars). For single-crate / lockstep this resolves to the
/// same tag the global context already carries, so behavior is unchanged.
///
/// Honors `--project-root` so tag discovery targets the release repo rather
/// than the process cwd.
pub fn resolve_crate_tag(ctx: &Context, crate_cfg: &CrateConfig) -> Option<String> {
    let monorepo_prefix = ctx.config.monorepo_tag_prefix();
    let repo = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let tag = crate::git::find_latest_tag_matching_with_prefix_in(
        &repo,
        &crate_cfg.tag_template,
        ctx.config.git.as_ref(),
        Some(ctx.template_vars()),
        monorepo_prefix,
    )
    .ok()
    .flatten()?;
    let stripped = match monorepo_prefix {
        Some(prefix) => crate::git::strip_monorepo_prefix(&tag, prefix).to_string(),
        None => tag,
    };
    Some(stripped)
}

/// Apply `(key, value)` overrides to `ctx`'s template vars, returning the
/// prior values (`None` when the key was absent) so the scope can be restored
/// by [`restore_var_overrides`].
pub fn apply_var_overrides(
    ctx: &mut Context,
    overrides: &[(&'static str, String)],
) -> Vec<(&'static str, Option<String>)> {
    let mut saved: Vec<(&'static str, Option<String>)> = Vec::new();
    let mut seen: HashSet<&'static str> = HashSet::new();
    for key in PER_CRATE_SCOPED_VARS {
        if seen.insert(*key) {
            saved.push((*key, ctx.template_vars().get(key).cloned()));
        }
    }
    for (key, value) in overrides {
        ctx.template_vars_mut().set(key, value);
    }
    saved
}

/// Restore template vars to the values captured by [`apply_var_overrides`].
pub fn restore_var_overrides(ctx: &mut Context, saved: Vec<(&'static str, Option<String>)>) {
    for (key, prior) in saved {
        match prior {
            Some(value) => ctx.template_vars_mut().set(key, &value),
            None => {
                ctx.template_vars_mut().unset(key);
            }
        }
    }
}

/// Re-scope `ctx`'s template vars to `crate_cfg` for the duration of `body`,
/// then restore them — even if `body` errors. The tag is resolved via
/// `resolve_tag` (production passes [`resolve_crate_tag`]; tests inject a
/// fixed-tag closure).
///
/// Fails loud when the crate has no resolvable tag or an unparseable one: a
/// per-crate emission stamped with the wrong version ships a broken,
/// hard-to-spot artifact, so the error must surface locally rather than be
/// papered over with the first crate's vars.
pub fn with_crate_scope<T>(
    ctx: &mut Context,
    crate_cfg: &CrateConfig,
    resolve_tag: &dyn Fn(&Context, &CrateConfig) -> Option<String>,
    body: impl FnOnce(&mut Context) -> Result<T>,
) -> Result<T> {
    let tag = resolve_tag(ctx, crate_cfg).with_context(|| {
        format!(
            "crate '{}' is selected for a per-crate emission but has no \
             release tag matching its tag_template; cannot derive its version",
            crate_cfg.name
        )
    })?;
    let overrides = crate_template_overrides(&crate_cfg.name, &tag)?;
    let saved = apply_var_overrides(ctx, &overrides);
    let result = body(ctx);
    restore_var_overrides(ctx, saved);
    result
}
