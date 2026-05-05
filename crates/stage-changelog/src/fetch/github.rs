//! GitHub Compare API commit fetcher (`use: github`).
//!
//! Lifted out of the umbrella `fetch/mod.rs` so the per-SCM JSON parsing
//! noise doesn't bloat the shared module. Calls into `super` for the
//! generic git-log fallback's helpers (none currently needed; left for
//! parallel structure with `gitlab.rs` / `gitea.rs`).

use std::collections::BTreeSet;

use anyhow::Result;

use anodizer_core::context::Context;
use anodizer_core::git::{detect_github_repo, gh_api_get, gh_api_get_paginated};
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
        let response = gh_api_get(&endpoint, token)?;
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
        (gh_api_get_paginated(&endpoint, token)?, None)
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
