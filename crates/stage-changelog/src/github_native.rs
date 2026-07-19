//! GitHub `releases/generate-notes` endpoint client.
//!
//! Used by the `changelog.use: github-native` flow to fetch GitHub's
//! auto-generated release notes upfront and embed them in the per-crate
//! changelog body.
//! GitHub's generate-release-notes endpoint:
//!
//! ```text
//! POST /repos/{owner}/{repo}/releases/generate-notes
//! { "tag_name": "<current>", "previous_tag_name": "<prev>" }
//! ```
//!
//! Calling this endpoint up front (vs. the lazier
//! `generate_release_notes: true` toggle on the create-release POST) is the
//! load-bearing parity decision: the dedicated endpoint accepts an
//! explicit `previous_tag_name`, which lets monorepos and re-releases pin
//! the commit range. The create-release POST flag silently uses GitHub's
//! "most recent published release" as the base — wrong for tag-prefixed
//! workflows.

use anyhow::{Context as _, Result, bail};
use std::process::Command;

use anodizer_core::config::{ChangelogConfig, ContentSource};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

use crate::run::{github_native_has_repo, resolve_prev_tag, write_changelog_dist};

/// Call `POST /repos/{owner}/{repo}/releases/generate-notes` via the `gh`
/// CLI and return the rendered release-notes body string.
///
/// `tag_name` is the current/target tag; `previous_tag_name` is optional —
/// when `None`, GitHub falls back to its default "previous release"
/// heuristic. The API itself is documented at
/// <https://docs.github.com/en/rest/releases/releases#generate-release-notes-content-for-a-release>.
pub(crate) fn generate_release_notes(
    owner: &str,
    repo: &str,
    tag_name: &str,
    previous_tag_name: Option<&str>,
    token: Option<&str>,
    log: &anodizer_core::log::StageLogger,
) -> Result<String> {
    // `tag_name` is a required field on `POST /repos/{owner}/{repo}/releases/
    // generate-notes` per the GitHub REST docs (Releases > Generate release
    // notes content for a release). Submitting an empty string surfaces as
    // a 422 (`tag_name is too short`) that hides the real cause: the
    // template rendered empty because `ctx.template_vars["Tag"]` was unset
    // on the snapshot / dry-run path.
    if tag_name.is_empty() {
        bail!(
            "changelog: github-native generate-notes for {}/{} is missing \
             required tag_name. GitHub POST /repos/{{owner}}/{{repo}}/releases/\
             generate-notes rejects empty `tag_name`. This usually means the \
             pipeline did not populate `Tag` in template vars (snapshot mode \
             without a `--tag` override). Re-run with an explicit tag or \
             configure `release.tag:` so the changelog stage can pin the \
             commit range.",
            owner,
            repo
        );
    }

    let mut body = serde_json::json!({ "tag_name": tag_name });
    if let Some(prev) = previous_tag_name {
        body["previous_tag_name"] = serde_json::Value::String(prev.to_string());
    }
    let body_str = serde_json::to_string(&body)?;

    let endpoint = format!("/repos/{}/{}/releases/generate-notes", owner, repo);
    let mut cmd = Command::new("gh");
    cmd.args(["api", "--method", "POST", &endpoint, "--input", "-"]);
    if let Some(tok) = token {
        cmd.env("GITHUB_TOKEN", tok);
    }

    let output = anodizer_core::run::run_checked_with_stdin(
        &mut cmd,
        body_str.as_bytes(),
        log,
        &format!("changelog: gh api POST {}", endpoint),
    )?;

    let response: serde_json::Value =
        serde_json::from_slice(&output.stdout).with_context(|| {
            format!(
                "changelog: failed to parse generate-notes response from {}",
                endpoint
            )
        })?;

    // Empty `body` is a documented success response: GitHub returns
    // 200 with `{ "body": "", ... }` when no commits / PRs sit between
    // `tag_name` and `previous_tag_name`. Treat the missing-key and
    // empty-string cases identically (per the REST endpoint contract:
    // "Generate release notes content for a release" returns an empty
    // body when there is nothing to summarise).
    let notes_body = response
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(notes_body)
}

/// Build the JSON request body that
/// [`generate_release_notes`] sends to GitHub. Extracted for unit-testing
/// the request shape without spawning `gh`.
#[cfg(test)]
pub(crate) fn build_request_body(
    tag_name: &str,
    previous_tag_name: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::json!({ "tag_name": tag_name });
    if let Some(prev) = previous_tag_name {
        body["previous_tag_name"] = serde_json::Value::String(prev.to_string());
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_includes_previous_tag_when_set() {
        let body = build_request_body("v2.0.0", Some("v1.0.0"));
        // When `previous_tag_name` is set, it is sent as a
        // top-level string field. GitHub's `/releases/generate-notes`
        // endpoint uses this as the "since" boundary for the commit range
        // — which is the load-bearing parity decision over the
        // create-release `generate_release_notes: true` flag (which uses
        // the most-recent published release as the base).
        assert_eq!(body["tag_name"], "v2.0.0");
        assert_eq!(body["previous_tag_name"], "v1.0.0");
    }

    #[test]
    fn request_body_omits_previous_tag_when_none() {
        let body = build_request_body("v2.0.0", None);
        // No previous_tag_name field — GitHub falls back to its default
        // "previous release" heuristic when
        // `ctx.Git.PreviousTag == ""` (first release).
        assert_eq!(body["tag_name"], "v2.0.0");
        assert!(body.get("previous_tag_name").is_none());
    }

    #[test]
    fn request_body_handles_monorepo_tag_prefix() {
        // Monorepo regression case: tag `service-a/v2.0.0` with previous
        // tag `service-a/v1.0.0` must round-trip verbatim — GitHub
        // accepts arbitrary tag strings, and the entire reason for using
        // this endpoint over `generate_release_notes: true` is to pin
        // such prefixed ranges reproducibly.
        let body = build_request_body("service-a/v2.0.0", Some("service-a/v1.0.0"));
        assert_eq!(body["tag_name"], "service-a/v2.0.0");
        assert_eq!(body["previous_tag_name"], "service-a/v1.0.0");
    }

    #[test]
    fn changelog_tag_name_empty_bails_with_actionable_error() {
        // GitHub `POST /repos/{owner}/{repo}/releases/generate-notes`
        // rejects empty `tag_name` with a 422 (`tag_name is too short`)
        // that hides the real cause: the snapshot path leaves `Tag`
        // unset in template vars. The helper must bail before spawning
        // `gh` so the user sees an actionable error.
        let log = anodizer_core::log::StageLogger::new("changelog", Default::default());
        let err = generate_release_notes("myorg", "myrepo", "", None, None, &log)
            .expect_err("empty tag_name must bail before spawning gh");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("changelog:"),
            "error must carry the changelog: prefix, got: {chain}"
        );
        assert!(
            chain.contains("tag_name"),
            "error must name the rejected field, got: {chain}"
        );
        assert!(
            chain.contains("myorg/myrepo"),
            "error must name the owner/repo, got: {chain}"
        );
        assert!(
            chain.contains("snapshot") || chain.contains("release.tag:"),
            "error must include an actionable hint, got: {chain}"
        );
    }
}

/// Whether a github-native generate-notes body should be treated as empty
/// (and therefore warned about, because the GitHub release will have no
/// real release notes).
///
/// GitHub's `generate-notes` endpoint returns 2xx with a body containing
/// only a `**Full Changelog**: …/compare/…` link when there are no merged
/// PRs between the two tags. That compare-only body carries no release
/// notes, so it is treated as empty here alongside genuinely blank /
/// whitespace-only bodies.
fn github_native_body_is_empty(body: &str) -> bool {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return true;
    }
    // A body whose every non-empty line is the auto-appended compare link
    // carries no actual release notes.
    trimmed
        .lines()
        .filter(|l| !l.trim().is_empty())
        .all(|l| l.trim_start().starts_with("**Full Changelog**:"))
}

/// Honour `--release-notes <path>`: read the file, fan it out to every
/// selected crate, and write `dist/CHANGELOG.md`. Returns `true` when the
/// override path was taken (caller short-circuits the rest of the stage).
pub(crate) fn handle_release_notes_override(ctx: &mut Context, log: &StageLogger) -> Result<bool> {
    let Some(notes_path) = ctx.options.release_notes_path.clone() else {
        return Ok(false);
    };
    let content = std::fs::read_to_string(&notes_path).with_context(|| {
        format!(
            "changelog: failed to read release notes file: {}",
            notes_path.display()
        )
    })?;
    if content.trim().is_empty() {
        log.warn(&format!(
            "release notes file {} is empty; release body will be blank",
            notes_path.display()
        ));
    }
    log.status(&format!(
        "using custom release notes from {}",
        notes_path.display()
    ));

    let selected = ctx.options.selected_crates.clone();
    let crates: Vec<_> = ctx
        .config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .cloned()
        .collect();
    for crate_cfg in &crates {
        ctx.stage_outputs
            .changelogs
            .insert(crate_cfg.name.clone(), content.clone());
    }

    let dist = ctx.config.dist.clone();
    std::fs::create_dir_all(&dist)
        .with_context(|| format!("changelog: create dist dir {}", dist.display()))?;
    let notes_out = dist.join("CHANGELOG.md");
    std::fs::write(&notes_out, &content)
        .with_context(|| format!("changelog: write {}", notes_out.display()))?;
    log.status(&format!("wrote {}", notes_out.display()));
    Ok(true)
}

/// Handle `use: github-native`: call GitHub's generate-notes endpoint per
/// crate, wrap with header/footer, and write `dist/CHANGELOG.md`. Mirrors
/// the github-native changelog backend.
pub(crate) fn handle_github_native_changelog(
    ctx: &mut Context,
    log: &StageLogger,
    changelog_cfg: Option<&ChangelogConfig>,
) -> Result<()> {
    if ctx.options.token.is_none() && !ctx.is_dry_run() && !ctx.is_snapshot() {
        bail!(
            "changelog: use=github-native requires a GitHub token ({}); \
             GitHub auto-release-notes is an authenticated API",
            anodizer_core::git::github_token_hint()
        );
    }
    if !github_native_has_repo(ctx) {
        // No crate in the current scope has a GitHub release configured
        // (e.g. a library-only workspace in per-crate publish-only). The
        // stage has nothing to fetch and nothing to write — return Ok
        // silently. A library workspace legitimately omits release.github;
        // there is nothing for the operator to fix, so a warn-level log
        // would be noise that pushes them to add a `skip: [changelog]`
        // toggle just to silence it.
        log.verbose(
            "skipped changelog — use=github-native and no crate in scope has \
             release.github (library workspace)",
        );
        return Ok(());
    }

    let monorepo_prefix = ctx.config.monorepo_tag_prefix();
    let current_tag = ctx.template_vars().get("Tag").cloned().unwrap_or_default();
    let token = ctx.options.token.clone();
    let dry_run_or_snapshot = ctx.is_dry_run() || ctx.is_snapshot();

    let selected = ctx.options.selected_crates.clone();
    let crates: Vec<_> = ctx
        .config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .cloned()
        .collect();

    let mut combined = String::new();
    // Tracks whether at least one per-crate body came from the real
    // GitHub generate-notes API (vs. all empty placeholders because
    // every crate lacked `release.github`). Used to set
    // `github_native_changelog` accurately — the flag signals "body
    // provenance is GitHub" to downstream consumers, so it must not be
    // set when every body is empty.
    let mut any_github_body = false;
    // Crates lacking `release.github` are collected here and reported in a
    // single aggregated warn after the loop — one line per workspace instead
    // of one per crate (a wide workspace would otherwise emit dozens).
    let mut missing_github: Vec<String> = Vec::new();
    // Crates whose github-native body came back empty in a REAL run are
    // collected here and reported in a single aggregated warn after the loop.
    // An empty body means the published GitHub release notes will be blank —
    // the operator must see this, so it is a warn (not status/verbose). This
    // is the silent-empty-release-notes outcome the warn makes visible.
    let mut empty_github_body: Vec<String> = Vec::new();
    for crate_cfg in &crates {
        let github_cfg = crate_cfg.release.as_ref().and_then(|r| r.github.as_ref());
        let Some(repo) = github_cfg else {
            missing_github.push(crate_cfg.name.clone());
            ctx.stage_outputs
                .changelogs
                .insert(crate_cfg.name.clone(), String::new());
            continue;
        };

        let current_tag_opt = if current_tag.is_empty() {
            None
        } else {
            Some(current_tag.as_str())
        };
        let prev_tag = resolve_prev_tag(ctx, crate_cfg, monorepo_prefix, current_tag_opt)?;

        let body = if dry_run_or_snapshot {
            log.status(&format!(
                "(dry-run/snapshot) would call POST /repos/{}/{}/releases/generate-notes \
                 (tag_name={:?}, previous_tag_name={:?})",
                repo.owner, repo.name, current_tag, prev_tag
            ));
            String::new()
        } else {
            log.status(&format!(
                "fetching github-native release notes for {}/{} \
                 (tag_name={:?}, previous_tag_name={:?})",
                repo.owner, repo.name, current_tag, prev_tag
            ));
            crate::github_native::generate_release_notes(
                &repo.owner,
                &repo.name,
                &current_tag,
                prev_tag.as_deref(),
                token.as_deref(),
                log,
            )
            .with_context(|| {
                format!(
                    "changelog: github-native generate-notes for crate '{}'",
                    crate_cfg.name
                )
            })?
        };

        ctx.stage_outputs
            .changelogs
            .insert(crate_cfg.name.clone(), body.clone());
        combined.push_str(&body);
        // generate-notes returns 2xx with empty content when no
        // PRs/commits sit between the two tags; only flip the
        // provenance flag in real runs when the body is non-empty so
        // downstream consumers aren't misled. In dry-run/snapshot the
        // body is always empty but the intent is still GitHub-sourced.
        if dry_run_or_snapshot || !body.trim().is_empty() {
            any_github_body = true;
        }
        // Real run + a crate that HAS release.github but got an empty body
        // back: github-native notes are PR-based, so this is the canonical
        // "no merged PRs between the two tags" case. Flag it loudly below so
        // the operator isn't surprised by a blank release body.
        if !dry_run_or_snapshot && github_native_body_is_empty(&body) {
            empty_github_body.push(crate_cfg.name.clone());
        }
    }

    // Aggregated skip warning: one line listing every crate without
    // `release.github`, so a wide workspace doesn't drown the log in
    // near-identical per-crate warnings. Warn (not status) so a genuinely
    // missing config stays visible in CI output.
    if !missing_github.is_empty() {
        log.warn(&format!(
            "skipped github-native notes (use=github-native) for {} \
             crate(s) without release.github: {} (if any should have a release, \
             add release.github.owner and release.github.name)",
            missing_github.len(),
            missing_github.join(", ")
        ));
    }

    // Aggregated empty-body warning: github-native generate-notes returned a
    // blank (or compare-link-only) body for these crates, so their GitHub
    // release notes will be EMPTY. github-native notes are PR-based, so the
    // usual cause is no merged PRs between previous_tag_name and tag_name
    // (e.g. a direct-push workflow). Point the operator at `changelog.use:
    // git` (or github/gitlab/gitea) for commit-based notes instead. Warn —
    // not status — because shipping silent empty release notes is exactly the
    // failure this surfaces.
    if !empty_github_body.is_empty() {
        log.warn(&format!(
            "use=github-native produced an EMPTY release body for {} \
             crate(s): {}. The GitHub release(s) will have NO release notes. \
             github-native notes are PR-based, so the likely cause is no merged \
             PRs between previous_tag_name and tag_name (e.g. a direct-push \
             workflow). Set `changelog.use: git` (or github/gitlab/gitea) for \
             commit-based release notes instead.",
            empty_github_body.len(),
            empty_github_body.join(", ")
        ));
    }

    ctx.stage_outputs.github_native_changelog = any_github_body;

    let header_src: Option<ContentSource> = changelog_cfg.and_then(|c| c.header.clone());
    let footer_src: Option<ContentSource> = changelog_cfg.and_then(|c| c.footer.clone());
    let header: Option<String> = header_src
        .as_ref()
        .map(|src| anodizer_core::content_source::resolve(src, "changelog header", ctx, log))
        .transpose()?;
    let footer: Option<String> = footer_src
        .as_ref()
        .map(|src| anodizer_core::content_source::resolve(src, "changelog footer", ctx, log))
        .transpose()?;

    // Header/footer in this path use unconditional emission semantics
    // (empty rendered content is still included with surrounding
    // whitespace), so it does not go through `wrap_with_header_footer`.
    let mut final_markdown = String::new();
    let mut rendered_header: Option<String> = None;
    if let Some(ref h) = header {
        let rendered = ctx.render_template_strict(h, "changelog header", log)?;
        if !rendered.trim().is_empty() {
            final_markdown.push_str(&rendered);
            final_markdown.push_str("\n\n");
            rendered_header = Some(rendered);
        }
    }
    final_markdown.push_str(&combined);
    let mut rendered_footer: Option<String> = None;
    if let Some(ref f) = footer {
        let rendered = ctx.render_template_strict(f, "changelog footer", log)?;
        if !rendered.trim().is_empty() {
            final_markdown.push('\n');
            final_markdown.push_str(&rendered);
            final_markdown.push('\n');
            rendered_footer = Some(rendered);
        }
    }
    ctx.stage_outputs.changelog_header = rendered_header;
    ctx.stage_outputs.changelog_footer = rendered_footer;

    let dist = ctx.config.dist.clone();
    write_changelog_dist(log, &dist, &final_markdown)
}

#[cfg(test)]
mod github_native_empty_body_tests {
    use super::github_native_body_is_empty;

    #[test]
    fn blank_body_is_empty() {
        assert!(github_native_body_is_empty(""));
        assert!(github_native_body_is_empty("   \n\t  \n"));
    }

    #[test]
    fn compare_link_only_body_is_empty() {
        // The exact shape generate-notes returns when there are no merged
        // PRs between the two tags (proven live against this repo's
        // v0.4.0..v0.5.0 range).
        let body = "**Full Changelog**: https://github.com/o/r/compare/v0.4.0...v0.5.0";
        assert!(
            github_native_body_is_empty(body),
            "compare-link-only body must warn → treated as empty"
        );
    }

    #[test]
    fn compare_link_with_surrounding_blank_lines_is_empty() {
        let body = "\n\n**Full Changelog**: https://github.com/o/r/compare/a...b\n\n";
        assert!(github_native_body_is_empty(body));
    }

    #[test]
    fn body_with_real_notes_is_not_empty() {
        let body = "## What's Changed\n* feat: add thing by @x in #1\n\n\
                    **Full Changelog**: https://github.com/o/r/compare/a...b";
        assert!(
            !github_native_body_is_empty(body),
            "a body with actual notes must NOT be treated as empty"
        );
    }
}
