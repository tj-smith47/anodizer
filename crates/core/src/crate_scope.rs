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
        crate_cfg.resolved_tag_template(),
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

/// Diagnostic for a crate whose `tag_template` matched no tag. The message
/// distinguishes a repo with ZERO tags (a state anodizer's own rollback / a
/// fresh or shallow clone manufactures — the template could never match, so
/// remedies matter more than the template) from a repo whose tags simply
/// don't match the template (where the template value and a sample of the
/// nearest existing tags are what the operator needs).
///
/// `selected_for` names the feature that demanded a per-crate version
/// ("version-sync/binstall", "a per-crate emission", …) so each call site
/// keeps its context while the diagnosis wording stays in one place.
pub fn no_matching_tag_error(ctx: &Context, crate_cfg: &CrateConfig, selected_for: &str) -> String {
    let repo = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let existing = crate::git::list_tags_with_prefix(&repo, "").unwrap_or_default();
    if existing.is_empty() {
        format!(
            "crate '{}' is selected for {selected_for} but has no release tag matching its \
             tag_template '{}': the repository has no git tags at all (likely a fresh clone \
             with no tags fetched, a shallow checkout, or tags deleted by a rollback/re-cut); \
             run `git fetch --tags`, create a release tag, or use --snapshot (local build) \
             or --nightly (synthesized version) which need no tag",
            crate_cfg.name,
            crate_cfg.resolved_tag_template()
        )
    } else {
        let sample = existing
            .iter()
            .take(5)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "crate '{}' is selected for {selected_for} but has no release tag matching its \
             tag_template '{}'; cannot derive its version (nearest existing tags: {sample})",
            crate_cfg.name,
            crate_cfg.resolved_tag_template()
        )
    }
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
    let tag = resolve_tag(ctx, crate_cfg)
        .with_context(|| no_matching_tag_error(ctx, crate_cfg, "a per-crate emission"))?;
    let overrides = crate_template_overrides(&crate_cfg.name, &tag)?;
    let saved = apply_var_overrides(ctx, &overrides);
    let result = body(ctx);
    restore_var_overrides(ctx, saved);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextOptions;
    use std::process::Command;

    fn ctx_at(root: &std::path::Path) -> Context {
        let options = ContextOptions {
            project_root: Some(root.to_path_buf()),
            ..Default::default()
        };
        Context::new(crate::config::Config::default(), options)
    }

    fn crate_cfg() -> CrateConfig {
        CrateConfig {
            name: "orphan".to_string(),
            path: ".".to_string(),
            tag_template: Some("orphan-v{{ .Version }}".to_string()),
            ..Default::default()
        }
    }

    fn git(root: &std::path::Path, args: &[&str]) {
        let out = crate::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.arg("-C")
                    .arg(root)
                    .args(args)
                    .env("GIT_TERMINAL_PROMPT", "0");
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// A repo with zero tags (rollback debris, fresh/shallow clone) must be
    /// diagnosed as such, with remedies, rather than as a template mismatch.
    #[test]
    fn no_matching_tag_error_names_tagless_repo_and_remedies() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        let msg = no_matching_tag_error(&ctx_at(tmp.path()), &crate_cfg(), "version-sync/binstall");
        assert!(
            msg.contains("no release tag matching its tag_template")
                && msg.contains("no git tags at all")
                && msg.contains("git fetch --tags")
                && msg.contains("--snapshot")
                && msg.contains("orphan-v{{ .Version }}"),
            "got: {msg}"
        );
    }

    /// When tags exist but none match the template, the message must show the
    /// template and a sample of the existing tags so the mismatch is visible.
    #[test]
    fn no_matching_tag_error_samples_existing_tags() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        git(
            tmp.path(),
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "x",
            ],
        );
        git(tmp.path(), &["tag", "widget-v1.2.3"]);
        git(tmp.path(), &["tag", "widget-v1.2.4"]);
        let msg = no_matching_tag_error(&ctx_at(tmp.path()), &crate_cfg(), "a per-crate emission");
        assert!(
            msg.contains("cannot derive its version")
                && msg.contains("orphan-v{{ .Version }}")
                && msg.contains("nearest existing tags:")
                && msg.contains("widget-v1.2.4"),
            "got: {msg}"
        );
    }
}
