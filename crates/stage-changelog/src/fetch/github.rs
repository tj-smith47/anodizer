//! GitHub Compare API commit fetcher (`use: github`).
//!
//! Lifted out of the umbrella `fetch/mod.rs` so the per-SCM JSON parsing
//! noise doesn't bloat the shared module. Calls into `super` for the
//! generic git-log fallback's helpers (none currently needed; left for
//! parallel structure with `gitlab.rs` / `gitea.rs`).

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;

use anodizer_core::context::Context;
use anodizer_core::git::{
    detect_github_repo, gh_api_get_paginated_with_binary, gh_api_get_with_binary,
};
use anodizer_core::log::StageLogger;

use crate::group::{CommitInfo, extract_co_authors, parse_commit_message};

// ---------------------------------------------------------------------------
// Helper: fetch commits from GitHub API (use: github)
// ---------------------------------------------------------------------------

/// Fetch commits via the GitHub API using the `gh` CLI.
/// Returns `(commits, logins_string)` where `logins_string` is a
/// comma-separated list of unique GitHub usernames.
///
/// When `path_filter` is set, commits are filtered to only those touching
/// files under the specified path (for monorepo support).
pub(crate) fn fetch_github_commits(
    ctx: &Context,
    prev_tag: &Option<String>,
    paths: &[String],
    log: &StageLogger,
) -> Result<(Vec<CommitInfo>, String)> {
    fetch_github_commits_with_binary(Path::new("gh"), ctx, prev_tag, paths, log)
}

/// Path-taking sibling of [`fetch_github_commits`].
///
/// `gh_binary` is the path to the `gh` CLI; pass `Path::new("gh")` for the
/// production PATH-lookup behavior. Tests point at a stub script inside a
/// `tempfile::tempdir()` to drive the parser without spawning the real CLI.
pub(crate) fn fetch_github_commits_with_binary(
    gh_binary: &Path,
    ctx: &Context,
    prev_tag: &Option<String>,
    paths: &[String],
    log: &StageLogger,
) -> Result<(Vec<CommitInfo>, String)> {
    let token = ctx.options.token.as_deref();
    let (owner, repo) = detect_github_repo()?;

    // Build the compare URL. If there is a previous tag, compare tag..HEAD;
    // otherwise list recent commits (first page).
    //
    // The Compare API returns a single JSON object (not a paginated array),
    // so we use `gh_api_get` instead of `gh_api_get_paginated` to avoid
    // corrupting the response by splitting on `]`.
    let (items, compare_files) = if let Some(tag) = prev_tag {
        let endpoint = format!("/repos/{owner}/{repo}/compare/{tag}...HEAD");
        let response = gh_api_get_with_binary(gh_binary, &endpoint, token)?;
        // Extract the "commits" array from the single compare object.
        let commits_arr = response
            .get("commits")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        // Extract the "files" array for path filtering.
        let files_arr = response
            .get("files")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        (commits_arr, Some(files_arr))
    } else {
        log.status("no previous tag, fetching recent commits from GitHub API");
        // The /commits endpoint returns a paginated array and supports ?path= natively.
        // GitHub API only supports a single path parameter, so use the first one.
        let mut endpoint = format!("/repos/{owner}/{repo}/commits?per_page=100");
        if let Some(first_path) = paths.first() {
            // URL-encode the path to handle spaces, #, ?, & etc.
            let mut encoded = String::with_capacity(first_path.len());
            for b in first_path.bytes() {
                match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                        encoded.push(b as char)
                    }
                    _ => encoded.push_str(&format!("%{:02X}", b)),
                }
            }
            endpoint.push_str(&format!("&path={}", encoded));
        }
        (
            gh_api_get_paginated_with_binary(gh_binary, &endpoint, token)?,
            None,
        )
    };

    // When using the Compare API with a path filter, filter commits to only
    // those that touched files under the specified paths.
    //
    // LIMITATION: The Compare API returns a flat "files" list for the entire
    // diff, not per-commit file lists. We can only check whether *any* changed
    // file matches *any* path prefix. If a match is found, ALL commits pass
    // through (we cannot determine which specific commits touched which files).
    // If no files match any path prefix, all commits are excluded.
    //
    // This is a coarser filter than the `git log -- path1 path2` approach used
    // by the git backend, which filters at the per-commit level. For precise
    // multi-path filtering, users should prefer `use: git` over `use: github`.
    let filtered_shas: Option<std::collections::HashSet<String>> = if !paths.is_empty() {
        if let Some(ref files) = compare_files {
            let has_matching_files = files.iter().any(|f| {
                f.get("filename")
                    .and_then(|v| v.as_str())
                    .is_some_and(|name| paths.iter().any(|p| name.starts_with(p.as_str())))
            });
            if !has_matching_files {
                Some(std::collections::HashSet::new()) // empty set = filter out all
            } else {
                None // no filtering needed, all commits are relevant
            }
        } else {
            None
        }
    } else {
        None
    };

    let mut logins = BTreeSet::new();
    let mut all_commit_infos = Vec::new();

    for item in &items {
        let sha = item.get("sha").and_then(|v| v.as_str()).unwrap_or_default();

        // When path filtering is active via the Compare API, skip commits that
        // don't match (empty set means no files matched the path prefix).
        if let Some(ref allowed) = filtered_shas
            && !allowed.contains(sha)
        {
            continue;
        }

        let short_sha = if sha.len() >= 7 { &sha[..7] } else { sha };
        let message = item
            .pointer("/commit/message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // Use first line of the commit message as the subject.
        let subject = message.lines().next().unwrap_or(message);
        let author_name = item
            .pointer("/commit/author/name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let author_email = item
            .pointer("/commit/author/email")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let login = item
            .pointer("/author/login")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if !login.is_empty() {
            logins.insert(login.to_string());
        }

        // Extract co-authors from the full commit message body.
        let co_authors = extract_co_authors(message);
        for co_author in &co_authors {
            // Co-authors don't have GitHub logins in the trailer, just names.
            // We still add them for visibility in the Logins variable.
            logins.insert(co_author.clone());
        }

        let mut info = parse_commit_message(subject);
        info.hash = short_sha.to_string();
        info.full_hash = sha.to_string();
        info.author_name = author_name.to_string();
        info.author_email = author_email.to_string();
        info.login = login.to_string();
        info.co_authors = co_authors;
        all_commit_infos.push(info);
    }

    let logins_str = logins.into_iter().collect::<Vec<_>>().join(",");
    Ok((all_commit_infos, logins_str))
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use anodizer_core::config::Config;
    use anodizer_core::context::ContextOptions;
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::CwdGuard;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    fn test_logger() -> StageLogger {
        StageLogger::new("changelog-github", Verbosity::Quiet)
    }

    /// Spin up a temp git repo with origin set to a fixed GitHub remote so
    /// `detect_github_repo()` resolves to ("myorg", "myrepo"). Returns the
    /// tempdir handle so the caller keeps it alive for the duration of the
    /// test.
    fn temp_github_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(path)
                .status()
                .expect("git init")
                .success()
        );
        assert!(
            Command::new("git")
                .args(["remote", "add", "origin", "git@github.com:myorg/myrepo.git",])
                .current_dir(path)
                .status()
                .expect("git remote add")
                .success()
        );
        dir
    }

    /// Write `body` to an executable shell script at `dir/gh` that emits
    /// `body` on stdout and exits 0. Returns the script path.
    fn write_gh_stub_stdout(dir: &Path, body: &str) -> std::path::PathBuf {
        let script = dir.join("gh");
        let contents = format!("#!/bin/sh\ncat <<'__GH_EOF__'\n{body}\n__GH_EOF__\n");
        std::fs::write(&script, contents).expect("write gh stub");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod gh stub");
        script
    }

    /// Build a Context with no token (default).
    fn test_ctx() -> Context {
        let config = Config {
            project_name: "myapp".to_string(),
            ..Config::default()
        };
        Context::new(config, ContextOptions::default())
    }

    /// Build a Context whose `options.token` is set so the redaction path
    /// in `gh_api_get_with_binary` actually has a token to redact.
    fn test_ctx_with_token(token: &str) -> Context {
        let config = Config {
            project_name: "myapp".to_string(),
            ..Config::default()
        };
        Context::new(
            config,
            ContextOptions {
                token: Some(token.to_string()),
                ..Default::default()
            },
        )
    }

    // ---- Compare API path (prev_tag is Some) ----

    #[test]
    #[serial_test::serial]
    fn compare_api_success_parses_two_commits() {
        let dir = tempfile::tempdir().expect("script dir");
        // Two commits + an empty files array (so path filtering is a no-op).
        let json = r#"{
            "commits": [
                {
                    "sha": "abc123def456abc123def456abc123def456abc1",
                    "commit": {
                        "message": "feat: first commit",
                        "author": {"name": "Ada", "email": "ada@example.com"}
                    },
                    "author": {"login": "ada"}
                },
                {
                    "sha": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                    "commit": {
                        "message": "fix: second commit",
                        "author": {"name": "Linus", "email": "linus@example.com"}
                    },
                    "author": {"login": "linus"}
                }
            ],
            "files": []
        }"#;
        let gh = write_gh_stub_stdout(dir.path(), json);

        let repo = temp_github_repo();
        let _cwd = CwdGuard::new(repo.path()).expect("cwd");
        let ctx = test_ctx();
        let log = test_logger();
        let (commits, logins) =
            fetch_github_commits_with_binary(&gh, &ctx, &Some("v1.0.0".to_string()), &[], &log)
                .expect("compare API parses");

        assert_eq!(commits.len(), 2);
        assert_eq!(
            commits[0].full_hash,
            "abc123def456abc123def456abc123def456abc1"
        );
        assert_eq!(commits[0].hash, "abc123d");
        assert_eq!(commits[0].author_name, "Ada");
        assert_eq!(commits[0].author_email, "ada@example.com");
        assert_eq!(commits[0].login, "ada");
        assert_eq!(commits[1].login, "linus");
        // BTreeSet ordering: ada, linus.
        assert_eq!(logins, "ada,linus");
    }

    #[test]
    #[serial_test::serial]
    fn compare_api_logins_are_unique_and_sorted() {
        let dir = tempfile::tempdir().expect("script dir");
        // Three commits, two by `ada` — logins set must de-dup.
        let json = r#"{
            "commits": [
                {"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","commit":{"message":"feat: a","author":{"name":"Ada","email":"a@x"}},"author":{"login":"ada"}},
                {"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","commit":{"message":"fix: b","author":{"name":"Ada","email":"a@x"}},"author":{"login":"ada"}},
                {"sha":"cccccccccccccccccccccccccccccccccccccccc","commit":{"message":"chore: c","author":{"name":"Bo","email":"b@x"}},"author":{"login":"bo"}}
            ],
            "files": []
        }"#;
        let gh = write_gh_stub_stdout(dir.path(), json);
        let repo = temp_github_repo();
        let _cwd = CwdGuard::new(repo.path()).expect("cwd");
        let ctx = test_ctx();
        let log = test_logger();
        let (commits, logins) =
            fetch_github_commits_with_binary(&gh, &ctx, &Some("v1.0.0".to_string()), &[], &log)
                .expect("parses");
        assert_eq!(commits.len(), 3);
        assert_eq!(logins, "ada,bo");
    }

    #[test]
    #[serial_test::serial]
    fn compare_api_path_filter_passes_through_when_match() {
        let dir = tempfile::tempdir().expect("script dir");
        // One commit; `files` includes "src/foo.rs"; filter on "src/" must
        // pass the commit through.
        let json = r#"{
            "commits": [
                {"sha":"abcabcabcabcabcabcabcabcabcabcabcabcabca","commit":{"message":"feat: x","author":{"name":"Ada","email":"a@x"}},"author":{"login":"ada"}}
            ],
            "files": [
                {"filename": "src/foo.rs"}
            ]
        }"#;
        let gh = write_gh_stub_stdout(dir.path(), json);
        let repo = temp_github_repo();
        let _cwd = CwdGuard::new(repo.path()).expect("cwd");
        let ctx = test_ctx();
        let log = test_logger();
        let (commits, _) = fetch_github_commits_with_binary(
            &gh,
            &ctx,
            &Some("v1.0.0".to_string()),
            &["src/".to_string()],
            &log,
        )
        .expect("parses");
        assert_eq!(commits.len(), 1);
    }

    #[test]
    #[serial_test::serial]
    fn compare_api_path_filter_excludes_all_when_no_match() {
        let dir = tempfile::tempdir().expect("script dir");
        // `files` only has src/foo.rs but the filter is "docs/" — every
        // commit must be filtered out.
        let json = r#"{
            "commits": [
                {"sha":"abcabcabcabcabcabcabcabcabcabcabcabcabca","commit":{"message":"feat: x","author":{"name":"Ada","email":"a@x"}},"author":{"login":"ada"}},
                {"sha":"defdefdefdefdefdefdefdefdefdefdefdefdefd","commit":{"message":"fix: y","author":{"name":"Bo","email":"b@x"}},"author":{"login":"bo"}}
            ],
            "files": [
                {"filename": "src/foo.rs"}
            ]
        }"#;
        let gh = write_gh_stub_stdout(dir.path(), json);
        let repo = temp_github_repo();
        let _cwd = CwdGuard::new(repo.path()).expect("cwd");
        let ctx = test_ctx();
        let log = test_logger();
        let (commits, _) = fetch_github_commits_with_binary(
            &gh,
            &ctx,
            &Some("v1.0.0".to_string()),
            &["docs/".to_string()],
            &log,
        )
        .expect("parses");
        assert!(
            commits.is_empty(),
            "no matching files must exclude every commit"
        );
    }

    // ---- /commits endpoint (prev_tag is None — paginated array) ----

    #[test]
    #[serial_test::serial]
    fn commits_endpoint_no_prev_tag_parses_two_commits() {
        let dir = tempfile::tempdir().expect("script dir");
        // Paginated endpoint returns a JSON array directly.
        let json = r#"[
            {"sha":"abc123def456abc123def456abc123def456abc1","commit":{"message":"feat: a","author":{"name":"Ada","email":"a@x"}},"author":{"login":"ada"}},
            {"sha":"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef","commit":{"message":"fix: b","author":{"name":"Linus","email":"l@x"}},"author":{"login":"linus"}}
        ]"#;
        let gh = write_gh_stub_stdout(dir.path(), json);
        let repo = temp_github_repo();
        let _cwd = CwdGuard::new(repo.path()).expect("cwd");
        let ctx = test_ctx();
        let log = test_logger();
        let (commits, logins) =
            fetch_github_commits_with_binary(&gh, &ctx, &None, &[], &log).expect("parses array");
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].hash, "abc123d");
        assert_eq!(logins, "ada,linus");
    }

    #[test]
    #[serial_test::serial]
    fn commits_endpoint_url_encodes_path_with_spaces() {
        // Stub `gh` writes the joined args to a fixed file inside the
        // script dir, then emits an empty JSON array so the parser
        // short-circuits. Asserts on the captured args.
        let dir = tempfile::tempdir().expect("script dir");
        let args_file = dir.path().join("gh-args.txt");
        let args_file_str = args_file.display().to_string();
        let script = dir.path().join("gh");
        let contents =
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{args_file_str}'\nprintf '[]\\n'\n");
        std::fs::write(&script, contents).expect("write gh stub");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod gh stub");

        let repo = temp_github_repo();
        let _cwd = CwdGuard::new(repo.path()).expect("cwd");
        let ctx = test_ctx();
        let log = test_logger();
        let (commits, _) = fetch_github_commits_with_binary(
            &script,
            &ctx,
            &None,
            &["dir with space/foo bar".to_string()],
            &log,
        )
        .expect("parses");
        assert!(commits.is_empty());

        let captured = std::fs::read_to_string(&args_file).expect("args file");
        // Expected substring per the in-source encoder: spaces -> %20,
        // slashes preserved.
        assert!(
            captured.contains("path=dir%20with%20space/foo%20bar"),
            "endpoint missing URL-encoded path; got args: {captured:?}"
        );
        // Sanity-check the API endpoint shape so a future refactor that
        // changes the per_page query or path joiner is caught.
        assert!(
            captured.contains("/repos/myorg/myrepo/commits?per_page=100"),
            "endpoint missing per_page param; got args: {captured:?}"
        );
    }

    // ---- Co-author extraction ----

    #[test]
    #[serial_test::serial]
    fn co_authors_are_extracted_and_added_to_logins() {
        let dir = tempfile::tempdir().expect("script dir");
        // Multi-line commit message with a Co-authored-by trailer. The
        // outer JSON encodes the \n as literal escape so the shell
        // heredoc preserves it.
        let json = r#"{
            "commits": [
                {
                    "sha": "abc123def456abc123def456abc123def456abc1",
                    "commit": {
                        "message": "feat: add thing\n\nDetails.\n\nCo-authored-by: Pair Programmer <pair@example.com>",
                        "author": {"name": "Ada", "email": "a@x"}
                    },
                    "author": {"login": "ada"}
                }
            ],
            "files": []
        }"#;
        let gh = write_gh_stub_stdout(dir.path(), json);
        let repo = temp_github_repo();
        let _cwd = CwdGuard::new(repo.path()).expect("cwd");
        let ctx = test_ctx();
        let log = test_logger();
        let (commits, logins) =
            fetch_github_commits_with_binary(&gh, &ctx, &Some("v1.0.0".to_string()), &[], &log)
                .expect("parses");
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].co_authors, vec!["Pair Programmer".to_string()]);
        // BTreeSet ordering of {"ada", "Pair Programmer"} — uppercase 'P'
        // sorts before lowercase 'a' in ASCII byte order.
        assert_eq!(logins, "Pair Programmer,ada");
    }

    // ---- Failure path: gh exits non-zero ----

    #[test]
    #[serial_test::serial]
    fn gh_failure_surfaces_error_with_redacted_token() {
        let dir = tempfile::tempdir().expect("script dir");
        let token = "ghp_secret_token_abc123";
        // Stub exits 1 with stderr that includes the token verbatim,
        // simulating a verbose gh error that echoed an auth header.
        let script = dir.path().join("gh");
        let contents = format!(
            "#!/bin/sh\nprintf 'HTTP 401: bad credentials token={token}\\n' 1>&2\nexit 1\n"
        );
        std::fs::write(&script, contents).expect("write gh stub");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod gh stub");

        let repo = temp_github_repo();
        let _cwd = CwdGuard::new(repo.path()).expect("cwd");
        let ctx = test_ctx_with_token(token);
        let log = test_logger();
        let err =
            fetch_github_commits_with_binary(&script, &ctx, &Some("v1.0.0".to_string()), &[], &log)
                .expect_err("non-zero gh exit must propagate");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("gh api GET"),
            "error must mention `gh api GET`, got: {chain}"
        );
        assert!(
            !chain.contains(token),
            "token leaked into error chain: {chain}"
        );
    }
}
