use anyhow::{Context as _, Result, bail};
use std::path::Path;
use std::process::Command;

use super::git_output_in;
use super::remote::detect_github_repo_in;
use super::tags::create_and_push_tag_in;

/// GET a GitHub API endpoint via the `gh` CLI (single request, no pagination).
///
/// Returns the parsed JSON response. Useful for endpoints that return a single
/// object (e.g. the Compare API) rather than a paginated array.
pub fn gh_api_get(endpoint: &str, token: Option<&str>) -> Result<serde_json::Value> {
    gh_api_get_with_binary(Path::new("gh"), endpoint, token)
}

/// GET a GitHub API endpoint via `gh_binary` (single request, no pagination).
///
/// Path-taking sibling of [`gh_api_get`] so tests can point at a missing or
/// stub binary inside a `tempfile::tempdir()` without mutating `PATH`.
/// When `gh_binary` has no separator (e.g. `Path::new("gh")`),
/// [`Command::new`] falls back to a PATH lookup — the production
/// behavior — so the wrapper is a true no-op in normal use.
pub fn gh_api_get_with_binary(
    gh_binary: &Path,
    endpoint: &str,
    token: Option<&str>,
) -> Result<serde_json::Value> {
    let mut cmd = Command::new(gh_binary);
    cmd.args(["api", endpoint]);
    if let Some(tok) = token {
        cmd.env("GITHUB_TOKEN", tok);
    }
    let output = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("failed to spawn gh CLI ({})", gh_binary.display()))?;
    if !output.status.success() {
        let stderr_raw = String::from_utf8_lossy(&output.stderr);
        let raw = format!("gh api GET {} failed: {}", endpoint, stderr_raw.trim());
        bail!("{}", redact_gh_stderr(&raw, token));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).context("failed to parse gh api response")
}

/// Redact secrets from `gh` CLI stderr before interpolating into a bail
/// message. `token` is the `GITHUB_TOKEN` value passed to the
/// subprocess; if the user-supplied token leaks (e.g. via a verbose `gh`
/// error that echoes the auth header), it is replaced with `$GITHUB_TOKEN`
/// regardless of whether the value matches the `redact::is_secret`
/// heuristics. Also strips inline URL credentials and any other secret
/// env-var values reachable from the parent process env.
fn redact_gh_stderr(stderr: &str, token: Option<&str>) -> String {
    let stripped = crate::redact::redact_url_credentials(stderr);
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    if let Some(tok) = token
        && !tok.is_empty()
    {
        env.push(("GITHUB_TOKEN".to_string(), tok.to_string()));
    }
    crate::redact::string(&stripped, &env)
}

/// GET a GitHub API endpoint via the `gh` CLI, with pagination.
///
/// Returns a JSON array of all pages concatenated. The caller is responsible for
/// ensuring that `gh` is installed and authenticated.
pub fn gh_api_get_paginated(endpoint: &str, token: Option<&str>) -> Result<Vec<serde_json::Value>> {
    gh_api_get_paginated_with_binary(Path::new("gh"), endpoint, token)
}

/// Paginated GET via `gh_binary`. Path-taking sibling of
/// [`gh_api_get_paginated`].
pub fn gh_api_get_paginated_with_binary(
    gh_binary: &Path,
    endpoint: &str,
    token: Option<&str>,
) -> Result<Vec<serde_json::Value>> {
    let mut cmd = Command::new(gh_binary);
    cmd.args(["api", "--paginate", endpoint]);
    if let Some(tok) = token {
        cmd.env("GITHUB_TOKEN", tok);
    }
    let output = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("failed to spawn gh CLI ({})", gh_binary.display()))?;

    if !output.status.success() {
        let stderr_raw = String::from_utf8_lossy(&output.stderr);
        let raw = format!("gh api GET {} failed: {}", endpoint, stderr_raw.trim());
        bail!("{}", redact_gh_stderr(&raw, token));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Try parsing the entire response first before falling back to splitting.
    // This avoids the split_inclusive(']') approach corrupting non-array responses.
    if let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(&stdout) {
        return Ok(arr);
    }
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&stdout) {
        // Single object response (e.g. non-list endpoint) — wrap in a vec.
        return Ok(vec![val]);
    }

    // Whole-parse failed — gh --paginate may return multiple JSON arrays
    // concatenated (e.g. `[...][...]`). Split on `]` boundaries and parse each chunk.
    let mut all_items = Vec::new();
    for chunk in stdout.split_inclusive(']') {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(serde_json::Value::Array(arr)) =
            serde_json::from_str::<serde_json::Value>(trimmed)
        {
            all_items.extend(arr);
        } else if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            all_items.push(val);
        } else {
            // Log unparseable chunks so corrupt data doesn't go unnoticed.
            // Route through `tracing::warn!` so subscriber-level redaction
            // applies (the chunk may carry templated request data). Cap
            // the logged chunk at 200 bytes — an HTTP body in an error
            // context should convey "what server said" without dumping a
            // multi-MB stack trace to the user's terminal.
            tracing::warn!(
                len = trimmed.len(),
                chunk = ?&trimmed[..trimmed.len().min(200)],
                "gh_api_get_paginated: failed to parse JSON chunk",
            );
        }
    }
    Ok(all_items)
}

/// POST via `gh_binary`. Internal helper consumed by
/// [`create_tag_via_github_api_in`]; takes an explicit binary path so
/// tests can drive the failure path against a missing or stub binary.
fn gh_api_post_with_binary(
    gh_binary: &Path,
    endpoint: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value> {
    use std::io::Write;

    let body_str = serde_json::to_string(body)?;

    let mut child = Command::new(gh_binary)
        .args(["api", "--method", "POST", endpoint, "--input", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn gh CLI ({})", gh_binary.display()))?;

    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(body_str.as_bytes())?;
    }
    child.stdin.take(); // close stdin

    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr_raw = String::from_utf8_lossy(&output.stderr);
        // `gh_api_post` does not currently accept a token argument, but
        // routing through `redact_process_env` still covers any token
        // exported as `GITHUB_TOKEN` / `GH_TOKEN` in the parent env, plus
        // inline URL credentials. Redact the full bail string so an
        // endpoint containing a secret-shaped path segment is also covered.
        let raw = format!("gh api POST {} failed: {}", endpoint, stderr_raw.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }

    let response: serde_json::Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("failed to parse GitHub API response from {}", endpoint))?;
    Ok(response)
}

/// Create a tag via the GitHub API (using the `gh` CLI).
///
/// This avoids the need for local git push access. Requires the `gh` CLI to be
/// installed and authenticated (`gh auth login`). The GitHub API creates a
/// lightweight tag object pointing at the HEAD commit on the default branch.
///
/// Falls back to [`create_and_push_tag_in`] if `gh` is not available.
pub fn create_tag_via_github_api(
    tag: &str,
    message: &str,
    dry_run: bool,
    log: &crate::log::StageLogger,
    strict: bool,
) -> Result<()> {
    create_tag_via_github_api_in(
        &std::env::current_dir()?,
        Path::new("gh"),
        tag,
        message,
        dry_run,
        log,
        strict,
    )
}

/// Path-taking sibling of [`create_tag_via_github_api`].
///
/// `cwd` is the repository the tag should be created against (used for
/// `git remote get-url origin` and `git rev-parse HEAD` lookups, plus
/// the local `git tag -a` fallback when `gh_binary` is missing).
/// `gh_binary` is the path to the `gh` CLI; pass `Path::new("gh")` to
/// keep the production PATH-lookup behavior.
#[allow(clippy::too_many_arguments)]
pub fn create_tag_via_github_api_in(
    cwd: &Path,
    gh_binary: &Path,
    tag: &str,
    message: &str,
    dry_run: bool,
    log: &crate::log::StageLogger,
    strict: bool,
) -> Result<()> {
    if dry_run {
        log.status(&format!(
            "(dry-run) would create tag via GitHub API: {} (\"{}\")",
            tag, message
        ));
        return Ok(());
    }

    // Detect owner/repo from the origin remote.
    let (owner, repo) = detect_github_repo_in(cwd)?;

    // Get the current HEAD SHA to point the tag at.
    let sha = git_output_in(cwd, &["rev-parse", "HEAD"])?;

    let body = serde_json::json!({
        "tag": tag,
        "message": message,
        "object": sha,
        "type": "commit",
        "tagger": {
            "name": git_output_in(cwd, &["config", "user.name"]).unwrap_or_else(|_| "anodizer".to_string()),
            "email": git_output_in(cwd, &["config", "user.email"]).unwrap_or_else(|_| "anodizer@users.noreply.github.com".to_string()),
            "date": crate::sde::resolve_now().to_rfc3339(),
        }
    });

    let tag_endpoint = format!("/repos/{owner}/{repo}/git/tags");
    let response = match gh_api_post_with_binary(gh_binary, &tag_endpoint, &body) {
        Ok(resp) => resp,
        Err(e) => {
            if e.to_string().contains("failed to spawn gh CLI") {
                if strict {
                    anyhow::bail!(
                        "gh CLI not found, cannot create tag via GitHub API (strict mode)"
                    );
                }
                log.warn("gh CLI not found, falling back to local git tag + push");
                return create_and_push_tag_in(cwd, tag, message, dry_run, log, strict);
            }
            return Err(e);
        }
    };

    let tag_sha = response["sha"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("GitHub API response missing 'sha' field"))?;

    let ref_body = serde_json::json!({
        "ref": format!("refs/tags/{}", tag),
        "sha": tag_sha,
    });

    let ref_endpoint = format!("/repos/{owner}/{repo}/git/refs");
    gh_api_post_with_binary(gh_binary, &ref_endpoint, &ref_body)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `dry_run=true` must short-circuit before any subprocess spawn.
    #[test]
    fn create_tag_dry_run_short_circuits() {
        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        // Even with no git repo / no gh CLI, dry-run must succeed.
        let result = create_tag_via_github_api("v1.0.0", "msg", true, &log, false);
        assert!(result.is_ok(), "dry-run must succeed: {result:?}");
    }

    /// Redact: the token must be replaced with the literal `$GITHUB_TOKEN`
    /// placeholder when it appears verbatim in the stderr output. Catches
    /// the case where `gh` echoes the auth header in a verbose error.
    #[test]
    fn redact_gh_stderr_replaces_token_value() {
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let stderr = format!("HTTP 401: token {secret} is invalid");
        let redacted = redact_gh_stderr(&stderr, Some(secret));
        assert!(
            !redacted.contains(secret),
            "token leaked into redacted output: {redacted}"
        );
    }

    #[test]
    fn redact_gh_stderr_with_no_token_still_strips_url_creds() {
        // Inline URL credentials must be redacted even with no explicit
        // token argument.
        let stderr = "auth failed: https://user:secret-pw@github.com/o/r.git rejected";
        let redacted = redact_gh_stderr(stderr, None);
        assert!(
            !redacted.contains("secret-pw"),
            "URL credential leaked: {redacted}"
        );
    }

    #[test]
    fn redact_gh_stderr_empty_token_is_noop_on_token_field() {
        // An empty Some("") token must not pollute the env vector with a
        // zero-length value (that would match every position in the string).
        let stderr = "plain error message without credentials";
        let redacted = redact_gh_stderr(stderr, Some(""));
        assert_eq!(redacted, stderr);
    }

    /// `gh_api_get_with_binary` must surface a user-actionable spawn
    /// failure when the binary path doesn't exist on disk.
    ///
    /// Drives the function with a temp-dir-relative path that points to
    /// nothing, asserting the error mentions "spawn gh" so the operator
    /// can correlate it with their missing-`gh` install state.
    #[test]
    fn gh_api_get_with_binary_bails_when_binary_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nonexistent-gh");
        let err = gh_api_get_with_binary(&missing, "/repos/x/y", None)
            .expect_err("missing binary must error");
        let msg = err.to_string();
        assert!(
            msg.contains("spawn gh") || msg.contains(&missing.display().to_string()),
            "expected actionable error mentioning spawn gh or the binary path, got: {msg}"
        );
    }

    /// Same guarantee for the paginated sibling.
    #[test]
    fn gh_api_get_paginated_with_binary_bails_when_binary_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nonexistent-gh");
        let err = gh_api_get_paginated_with_binary(&missing, "/repos/x/y", None)
            .expect_err("missing binary must error");
        let msg = err.to_string();
        assert!(
            msg.contains("spawn gh") || msg.contains(&missing.display().to_string()),
            "expected actionable error mentioning spawn gh or the binary path, got: {msg}"
        );
    }

    /// `create_tag_via_github_api_in` with `strict=true` must error
    /// when `cwd` is not a git repo — the inner `detect_github_repo_in`
    /// drives `git remote get-url origin` and fails there.
    ///
    /// Skips when `git` isn't on PATH (mirrors `tool_on_path` patterns
    /// elsewhere in the suite).
    #[test]
    fn create_tag_via_github_api_in_bails_when_not_a_git_repo() {
        if Command::new("git")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        let err = create_tag_via_github_api_in(
            tmp.path(),
            Path::new("gh"),
            "v1.0.0",
            "msg",
            false,
            &log,
            true,
        )
        .expect_err("non-git cwd must error");
        let msg = err.to_string();
        assert!(
            msg.contains("git") || msg.contains("remote"),
            "expected error to mention git or remote, got: {msg}"
        );
    }

    /// Dry-run short-circuit must also fire on the cwd-injectable entry
    /// point — covers the new branch without re-hitting the inner
    /// detection codepath.
    #[test]
    fn create_tag_via_github_api_in_dry_run_short_circuits() {
        let tmp = tempfile::tempdir().unwrap();
        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        let result = create_tag_via_github_api_in(
            tmp.path(),
            Path::new("gh"),
            "v1.0.0",
            "msg",
            true,
            &log,
            false,
        );
        assert!(result.is_ok(), "dry-run must succeed: {result:?}");
    }
}
