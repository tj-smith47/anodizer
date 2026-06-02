//! GitHub `releases/generate-notes` endpoint client.
//!
//! Used by the `changelog.use: github-native` flow to fetch GitHub's
//! auto-generated release notes upfront and embed them in the per-crate
//! changelog body. Mirrors GoReleaser
//! `internal/client/github.go::GenerateReleaseNotes`
//! (`internal/pipe/changelog/changelog.go::githubNativeChangeloger.Log`):
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
) -> Result<String> {
    use std::io::Write;

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

    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("changelog: failed to spawn gh CLI for generate-notes")?;

    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(body_str.as_bytes())?;
    }
    child.stdin.take(); // close stdin

    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "changelog: gh api POST {} failed: {}",
            endpoint,
            stderr.trim()
        );
    }

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
        // GR's contract: when `previous_tag_name` is set, it is sent as a
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
        // "previous release" heuristic. Mirrors GR's behaviour when
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
        let err = generate_release_notes("myorg", "myrepo", "", None, None)
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
