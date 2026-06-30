use anyhow::{Context as _, Result, bail};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use super::git_output_in;
use super::slug::RepoSlug;
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

/// Cache key for [`COMMIT_LOGIN_CACHE`]: `(owner, repo, author_email)`.
type LoginCacheKey = (String, String, String);

/// Process-wide memo of commit-author login lookups. Failed lookups are
/// cached as `None` so an offline / unauthenticated run costs at most one
/// API attempt per unique author email, even when several CLI entry points
/// render changelogs for many crates in the same invocation.
static COMMIT_LOGIN_CACHE: OnceLock<Mutex<HashMap<LoginCacheKey, Option<String>>>> =
    OnceLock::new();

/// Resolve a commit author's GitHub login from a representative commit SHA
/// via `GET /repos/{owner}/{repo}/commits/{sha}` → `.author.login`.
///
/// Best-effort by design: any failure (no `gh`, no auth, offline, unknown
/// SHA, commit email not linked to a GitHub account) returns `None` with at
/// most a debug-level trace — callers fall back to name-based rendering and
/// must never fail a release pipeline over a missing login.
///
/// Results (including failures) are memoized process-wide per
/// `(owner, repo, email)`, so each unique author email costs one API call
/// per run regardless of how many commits or crates reference it.
pub fn commit_author_login(
    owner: &str,
    repo: &str,
    email: &str,
    sha: &str,
    token: Option<&str>,
) -> Option<String> {
    commit_author_login_with_binary(Path::new("gh"), owner, repo, email, sha, token)
}

/// Path-taking sibling of [`commit_author_login`] so tests can point at a
/// missing or stub binary without mutating `PATH`.
pub fn commit_author_login_with_binary(
    gh_binary: &Path,
    owner: &str,
    repo: &str,
    email: &str,
    sha: &str,
    token: Option<&str>,
) -> Option<String> {
    if owner.is_empty() || repo.is_empty() || email.is_empty() || sha.is_empty() {
        return None;
    }
    let key = (owner.to_string(), repo.to_string(), email.to_string());
    let cache = COMMIT_LOGIN_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    // A poisoned lock only means another thread panicked mid-insert; the map
    // itself is still a valid memo, so recover it rather than panic here.
    {
        let guard = match cache.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(hit) = guard.get(&key) {
            return hit.clone();
        }
    }
    let endpoint = format!("/repos/{owner}/{repo}/commits/{sha}");
    let resolved = match gh_api_get_with_binary(gh_binary, &endpoint, token) {
        Ok(v) => v
            .pointer("/author/login")
            .and_then(|l| l.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        Err(e) => {
            tracing::debug!(
                "commit_author_login: lookup for {} failed (keeping name-based rendering): {}",
                email,
                e
            );
            None
        }
    };
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.insert(key, resolved.clone());
    resolved
}

/// The ordered env-var ladder consulted (after any explicit CLI/context
/// value) when resolving a GitHub token: `ANODIZER_GITHUB_TOKEN` is preferred,
/// then `GITHUB_TOKEN`. This is the single source of truth for the ladder —
/// [`resolve_github_token_with_env`] (the only real reader) consumes it, and
/// the config-aware preflight builds its `EnvAnyOf` lists from it so the
/// validated set cannot drift from the set the resolver actually reads.
pub const GITHUB_TOKEN_ENV_LADDER: &[&str] = &["ANODIZER_GITHUB_TOKEN", "GITHUB_TOKEN"];

/// Resolve the GitHub token for API calls through the codebase-standard
/// chain: explicit value (CLI flag / context option) → the
/// [`GITHUB_TOKEN_ENV_LADDER`] (`ANODIZER_GITHUB_TOKEN` → `GITHUB_TOKEN`).
/// Empty strings count as absent at every link — GitHub Actions materializes
/// missing secrets as `""`, which must not short-circuit the fallback to the
/// next link.
///
/// `env` is the injectable env source (pass `Context::env_var` or a
/// map-backed closure in tests) so the chain is testable without mutating
/// process env.
pub fn resolve_github_token_with_env(
    explicit: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let non_empty = |s: String| if s.is_empty() { None } else { Some(s) };
    let explicit = explicit.filter(|t| !t.is_empty()).map(str::to_string);
    GITHUB_TOKEN_ENV_LADDER.iter().fold(explicit, |acc, var| {
        acc.or_else(|| env(var).and_then(non_empty))
    })
}

/// Process-env wrapper of [`resolve_github_token_with_env`] for call sites
/// without a `Context` (e.g. the `bump`/`tag` changelog-sync write path,
/// whose commands expose no `--token` flag).
pub fn resolve_github_token(explicit: Option<&str>) -> Option<String> {
    resolve_github_token_with_env(explicit, &|key| std::env::var(key).ok())
}

/// Redact secrets from `gh` CLI stderr before interpolating into a bail
/// message. `token` is the `GITHUB_TOKEN` value passed to the
/// subprocess; if the user-supplied token leaks (e.g. via a verbose `gh`
/// error that echoes the auth header), it is replaced with `$GITHUB_TOKEN`
/// regardless of whether the value matches the `redact::is_secret`
/// heuristics. Also strips inline URL credentials and any other secret
/// env-var values reachable from the parent process env.
fn redact_gh_stderr(stderr: &str, token: Option<&str>) -> String {
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    if let Some(tok) = token
        && !tok.is_empty()
    {
        env.push(("GITHUB_TOKEN".to_string(), tok.to_string()));
    }
    crate::redact::with_env(stderr, &env)
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
            // The chunk may carry secret-shaped request/response data, and the
            // tracing subscriber performs NO redaction of its own — so redact
            // here (process-env secret values + inline URL credentials) before
            // emitting. Cap the logged chunk at 200 bytes — an HTTP body in an
            // error context should convey "what server said" without dumping a
            // multi-MB stack trace to the user's terminal.
            let snippet = &trimmed[..trimmed.len().min(200)];
            let redacted = crate::redact::redact_process_env(snippet);
            tracing::warn!(
                "gh_api_get_paginated: failed to parse JSON chunk ({} bytes): {:?}",
                trimmed.len(),
                redacted,
            );
        }
    }

    // A non-empty body that yielded zero items means the whole-parse failed AND
    // every chunk failed to parse — garbled stdout from a zero-exit `gh`. Returning
    // an empty vec here would be indistinguishable from a genuine "no results",
    // letting a caller wrongly conclude a release/asset is absent. Fail loud instead.
    if all_items.is_empty() && !stdout.trim().is_empty() {
        let raw = format!(
            "gh api GET {endpoint} exited 0 but returned a body that could not be parsed as JSON"
        );
        bail!("{}", redact_gh_stderr(&raw, token));
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
    log: &crate::log::StageLogger,
) -> Result<serde_json::Value> {
    let body_str = serde_json::to_string(body)?;

    let mut cmd = Command::new(gh_binary);
    cmd.args(["api", "--method", "POST", endpoint, "--input", "-"]);

    // `gh api` may echo a token from the parent env (`GITHUB_TOKEN` /
    // `GH_TOKEN`) on stderr; carry the full process env on a logger clone so
    // the helper's redaction matches the prior `redact_process_env` coverage
    // (broader than this caller's attached env). The "gh CLI" label keeps the
    // spawn-failure string the caller pattern-matches on
    // (`failed to spawn gh CLI`) for its git-fallback decision.
    let redacting_log = log.clone().with_env(std::env::vars().collect::<Vec<_>>());
    let output = crate::run::run_checked_with_stdin(
        &mut cmd,
        body_str.as_bytes(),
        &redacting_log,
        "gh CLI",
    )?;

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
///
/// `slug` is the resolved repository identity (config override -> remote),
/// supplied by the caller so the API target and the rest of the pipeline agree
/// on one owner/repo rather than this function re-deriving its own.
pub fn create_tag_via_github_api(
    slug: &RepoSlug,
    tag: &str,
    message: &str,
    dry_run: bool,
    log: &crate::log::StageLogger,
    strict: bool,
) -> Result<()> {
    create_tag_via_github_api_in(
        &std::env::current_dir()?,
        Path::new("gh"),
        slug,
        tag,
        message,
        dry_run,
        log,
        strict,
    )
}

/// Path-taking sibling of [`create_tag_via_github_api`].
///
/// `cwd` is the repository the tag should be created against (used for the
/// `git rev-parse HEAD` lookup and the local `git tag -a` fallback when
/// `gh_binary` is missing). `slug` is the resolved owner/repo. `gh_binary` is
/// the path to the `gh` CLI; pass `Path::new("gh")` to keep the production
/// PATH-lookup behavior.
#[allow(clippy::too_many_arguments)]
pub fn create_tag_via_github_api_in(
    cwd: &Path,
    gh_binary: &Path,
    slug: &RepoSlug,
    tag: &str,
    message: &str,
    dry_run: bool,
    log: &crate::log::StageLogger,
    strict: bool,
) -> Result<()> {
    if dry_run {
        log.status(&format!(
            "(dry-run) would create tag {} via GitHub API (\"{}\")",
            tag, message
        ));
        return Ok(());
    }

    let owner = slug.owner();
    let repo = slug.name();

    // Get the current HEAD SHA to point the tag at — via the canonical
    // single sha resolver, not a private `rev-parse` here.
    let sha = super::get_head_commit_in(cwd)?;

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
    let response = match gh_api_post_with_binary(gh_binary, &tag_endpoint, &body, log) {
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
    gh_api_post_with_binary(gh_binary, &ref_endpoint, &ref_body, log)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `dry_run=true` must short-circuit before any subprocess spawn.
    #[test]
    fn create_tag_dry_run_short_circuits() {
        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        let slug = RepoSlug::for_test("owner", "repo");
        // Even with no git repo / no gh CLI, dry-run must succeed.
        let result = create_tag_via_github_api(&slug, "v1.0.0", "msg", true, &log, false);
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
        // Exact-output, not absence-only: guards the env-value masking
        // layer — the token is replaced by its `$NAME` placeholder.
        assert_eq!(redacted, "HTTP 401: token $GITHUB_TOKEN is invalid");
    }

    #[test]
    fn redact_gh_stderr_with_no_token_still_strips_url_creds() {
        // Inline URL credentials must be redacted even with no explicit
        // token argument.
        let stderr = "auth failed: https://user:secret-pw@github.com/o/r.git rejected";
        let redacted = redact_gh_stderr(stderr, None);
        // Exact-output, not absence-only: guards the inline URL-credential
        // stripping layer — userinfo becomes `<redacted>`.
        assert_eq!(
            redacted,
            "auth failed: https://<redacted>@github.com/o/r.git rejected"
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

    /// A missing `gh` binary must degrade to `None` (never an error/panic):
    /// the changelog pipeline keeps name-based rendering. The failure is
    /// memoized, so the second call returns the cached `None` without
    /// re-attempting a spawn.
    #[test]
    fn commit_author_login_missing_binary_degrades_to_none_and_caches() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nonexistent-gh");
        let first = commit_author_login_with_binary(
            &missing,
            "owner-cal-test",
            "repo-cal-test",
            "a@example.com",
            "0123456789abcdef0123456789abcdef01234567",
            None,
        );
        assert_eq!(first, None, "missing binary must yield None");
        // Cached-failure path: same (owner, repo, email) key short-circuits
        // before any spawn attempt.
        let second = commit_author_login_with_binary(
            &missing,
            "owner-cal-test",
            "repo-cal-test",
            "a@example.com",
            "fedcba9876543210fedcba9876543210fedcba98",
            None,
        );
        assert_eq!(second, None);
    }

    /// Empty inputs short-circuit to `None` without touching the cache or
    /// spawning anything.
    #[test]
    fn commit_author_login_empty_inputs_are_none() {
        let gh = Path::new("gh");
        assert_eq!(
            commit_author_login_with_binary(gh, "", "r", "e", "s", None),
            None
        );
        assert_eq!(
            commit_author_login_with_binary(gh, "o", "", "e", "s", None),
            None
        );
        assert_eq!(
            commit_author_login_with_binary(gh, "o", "r", "", "s", None),
            None
        );
        assert_eq!(
            commit_author_login_with_binary(gh, "o", "r", "e", "", None),
            None
        );
    }

    /// Chain order: explicit beats `ANODIZER_GITHUB_TOKEN` beats
    /// `GITHUB_TOKEN`; empty strings are absent at every link. Uses a
    /// map-backed env closure — no process-env mutation, no network.
    #[test]
    fn resolve_github_token_chain_order_and_empty_filtering() {
        let env_with = |pairs: &[(&str, &str)]| {
            let map: HashMap<String, String> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            move |key: &str| map.get(key).cloned()
        };

        let both = env_with(&[
            ("ANODIZER_GITHUB_TOKEN", "anod-tok"),
            ("GITHUB_TOKEN", "gh-tok"),
        ]);
        assert_eq!(
            resolve_github_token_with_env(Some("explicit-tok"), &both),
            Some("explicit-tok".to_string()),
            "explicit token must win over both env vars"
        );
        assert_eq!(
            resolve_github_token_with_env(None, &both),
            Some("anod-tok".to_string()),
            "ANODIZER_GITHUB_TOKEN must win over GITHUB_TOKEN"
        );

        let gh_only = env_with(&[("GITHUB_TOKEN", "gh-tok")]);
        assert_eq!(
            resolve_github_token_with_env(None, &gh_only),
            Some("gh-tok".to_string()),
            "GITHUB_TOKEN alone must resolve (the CI-gap pin)"
        );
        assert_eq!(
            resolve_github_token_with_env(Some(""), &gh_only),
            Some("gh-tok".to_string()),
            "empty explicit token must fall through to env"
        );

        let empty_anod = env_with(&[("ANODIZER_GITHUB_TOKEN", ""), ("GITHUB_TOKEN", "gh-tok")]);
        assert_eq!(
            resolve_github_token_with_env(None, &empty_anod),
            Some("gh-tok".to_string()),
            "empty ANODIZER_GITHUB_TOKEN (GHA missing-secret materialization) must not short-circuit"
        );

        let none = env_with(&[]);
        assert_eq!(resolve_github_token_with_env(None, &none), None);
        assert_eq!(resolve_github_token_with_env(Some(""), &none), None);
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
    /// when `cwd` is not a git repo — the HEAD-sha resolver
    /// (`get_head_commit_in`) drives `git rev-parse HEAD` and fails there.
    ///
    /// Skips when `git` isn't on PATH (mirrors `tool_on_path` patterns
    /// elsewhere in the suite).
    #[test]
    fn create_tag_via_github_api_in_bails_when_not_a_git_repo() {
        // spawn-retry-ok: this is a git *availability* probe — an Err means git
        // is absent (skip the test via the success() guard), not a transient
        // spawn-init failure to retry; routing it through the panicking helper
        // would crash on a git-less host instead of skipping.
        if !Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Quiet);
        let slug = RepoSlug::for_test("owner", "repo");
        let err = create_tag_via_github_api_in(
            tmp.path(),
            Path::new("gh"),
            &slug,
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
        let slug = RepoSlug::for_test("owner", "repo");
        let result = create_tag_via_github_api_in(
            tmp.path(),
            Path::new("gh"),
            &slug,
            "v1.0.0",
            "msg",
            true,
            &log,
            false,
        );
        assert!(result.is_ok(), "dry-run must succeed: {result:?}");
    }
}
