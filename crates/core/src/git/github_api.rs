use anyhow::{Context as _, Result, bail};
use std::process::Command;

use super::git_output;
use super::remote::detect_github_repo;
use super::tags::create_and_push_tag;

/// GET a GitHub API endpoint via the `gh` CLI (single request, no pagination).
///
/// Returns the parsed JSON response. Useful for endpoints that return a single
/// object (e.g. the Compare API) rather than a paginated array.
pub fn gh_api_get(endpoint: &str, token: Option<&str>) -> Result<serde_json::Value> {
    let mut cmd = Command::new("gh");
    cmd.args(["api", endpoint]);
    if let Some(tok) = token {
        cmd.env("GITHUB_TOKEN", tok);
    }
    let output = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("failed to spawn gh CLI")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api GET {} failed: {}", endpoint, stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).context("failed to parse gh api response")
}

/// GET a GitHub API endpoint via the `gh` CLI, with pagination.
///
/// Returns a JSON array of all pages concatenated. The caller is responsible for
/// ensuring that `gh` is installed and authenticated.
pub fn gh_api_get_paginated(endpoint: &str, token: Option<&str>) -> Result<Vec<serde_json::Value>> {
    let mut cmd = Command::new("gh");
    cmd.args(["api", "--paginate", endpoint]);
    if let Some(tok) = token {
        cmd.env("GITHUB_TOKEN", tok);
    }
    let output = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("failed to spawn gh CLI")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api GET {} failed: {}", endpoint, stderr.trim());
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
            eprintln!(
                "warning: gh_api_get_paginated: failed to parse JSON chunk (len={}): {:?}",
                trimmed.len(),
                &trimmed[..trimmed.len().min(200)]
            );
        }
    }
    Ok(all_items)
}

/// POST a JSON body to a GitHub API endpoint via the `gh` CLI.
///
/// Returns the parsed JSON response on success. The caller is responsible for
/// ensuring that `gh` is installed and authenticated.
fn gh_api_post(endpoint: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    use std::io::Write;

    let body_str = serde_json::to_string(body)?;

    let mut child = Command::new("gh")
        .args(["api", "--method", "POST", endpoint, "--input", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn gh CLI")?;

    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(body_str.as_bytes())?;
    }
    child.stdin.take(); // close stdin

    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api POST {} failed: {}", endpoint, stderr.trim());
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
/// Falls back to [`create_and_push_tag`] if `gh` is not available.
pub fn create_tag_via_github_api(
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
    let (owner, repo) = detect_github_repo()?;

    // Get the current HEAD SHA to point the tag at.
    let sha = git_output(&["rev-parse", "HEAD"])?;

    // Step 1: Create the tag object
    let body = serde_json::json!({
        "tag": tag,
        "message": message,
        "object": sha,
        "type": "commit",
        "tagger": {
            "name": git_output(&["config", "user.name"]).unwrap_or_else(|_| "anodizer".to_string()),
            "email": git_output(&["config", "user.email"]).unwrap_or_else(|_| "anodizer@users.noreply.github.com".to_string()),
            "date": chrono::Utc::now().to_rfc3339(),
        }
    });

    let tag_endpoint = format!("/repos/{owner}/{repo}/git/tags");
    let response = match gh_api_post(&tag_endpoint, &body) {
        Ok(resp) => resp,
        Err(e) => {
            if e.to_string().contains("failed to spawn gh CLI") {
                if strict {
                    anyhow::bail!(
                        "gh CLI not found, cannot create tag via GitHub API (strict mode)"
                    );
                }
                log.warn("gh CLI not found, falling back to local git tag + push");
                return create_and_push_tag(tag, message, dry_run, log, strict);
            }
            return Err(e);
        }
    };

    let tag_sha = response["sha"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("GitHub API response missing 'sha' field"))?;

    // Step 2: Create the ref pointing to the tag object
    let ref_body = serde_json::json!({
        "ref": format!("refs/tags/{}", tag),
        "sha": tag_sha,
    });

    let ref_endpoint = format!("/repos/{owner}/{repo}/git/refs");
    gh_api_post(&ref_endpoint, &ref_body)?;

    Ok(())
}
