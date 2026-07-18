use super::*;
use anyhow::Result;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Commit {
    pub hash: String,
    pub short_hash: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    /// Full commit message body (everything after the subject line).
    /// Contains trailers like `Co-Authored-By:`.
    pub body: String,
}

/// Parse git log output (formatted as `%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e`)
/// into a vec of [`Commit`]s.
///
/// Uses ASCII record separator (0x1e) between commits and unit separator (0x1f)
/// between fields, so multi-line body text doesn't break parsing.
///
/// The single record decoder for this wire format: every changelog path
/// (`parse_commit_output_with_files` here, and the changelog stage's git
/// fetch) decodes through this function so the body / author fields can never
/// drift between call sites.
pub fn parse_commit_output(output: &str) -> Vec<Commit> {
    if output.is_empty() {
        return vec![];
    }
    output
        .split('\x1e')
        .filter(|record| !record.trim().is_empty())
        .filter_map(|record| {
            let fields: Vec<&str> = record.split('\x1f').collect();
            if fields.len() >= 5 {
                Some(Commit {
                    hash: fields[0].trim().to_string(),
                    short_hash: fields[1].to_string(),
                    message: fields[2].to_string(),
                    author_name: fields[3].to_string(),
                    author_email: fields[4].to_string(),
                    body: fields.get(5).unwrap_or(&"").trim().to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

pub(super) fn cwd_or_dot() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Get commits between two refs, optionally filtered to a path.
pub fn get_commits_between(from: &str, to: &str, path_filter: Option<&str>) -> Result<Vec<Commit>> {
    get_commits_between_in(&cwd_or_dot(), from, to, path_filter)
}

/// Path-taking sibling of [`get_commits_between`].
pub fn get_commits_between_in(
    cwd: &Path,
    from: &str,
    to: &str,
    path_filter: Option<&str>,
) -> Result<Vec<Commit>> {
    get_commits_between_paths_in(
        cwd,
        from,
        to,
        &path_filter
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>(),
    )
}

/// Get commits between two refs, filtered to multiple paths (git log -- path1 path2 ...).
pub fn get_commits_between_paths(from: &str, to: &str, paths: &[String]) -> Result<Vec<Commit>> {
    get_commits_between_paths_in(&cwd_or_dot(), from, to, paths)
}

/// Path-taking sibling of [`get_commits_between_paths`].
pub fn get_commits_between_paths_in(
    cwd: &Path,
    from: &str,
    to: &str,
    paths: &[String],
) -> Result<Vec<Commit>> {
    let range = format!("{}..{}", from, to);
    let mut args = vec![
        "-c".to_string(),
        "log.showSignature=false".to_string(),
        "log".to_string(),
        "--pretty=format:%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e".to_string(),
        range,
    ];
    if !paths.is_empty() {
        args.push("--".to_string());
        for p in paths {
            args.push(p.clone());
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = git_output_in(cwd, &arg_refs)?;
    Ok(parse_commit_output(&output))
}

/// Get all commits reachable from HEAD, optionally filtered to a path.
/// Used for initial releases where there is no previous tag.
pub fn get_all_commits(path_filter: Option<&str>) -> Result<Vec<Commit>> {
    get_all_commits_in(&cwd_or_dot(), path_filter)
}

/// Path-taking sibling of [`get_all_commits`].
pub fn get_all_commits_in(cwd: &Path, path_filter: Option<&str>) -> Result<Vec<Commit>> {
    get_all_commits_paths_in(
        cwd,
        &path_filter
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>(),
    )
}

/// Get all commits reachable from HEAD, filtered to multiple paths.
pub fn get_all_commits_paths(paths: &[String]) -> Result<Vec<Commit>> {
    get_all_commits_paths_in(&cwd_or_dot(), paths)
}

/// Path-taking sibling of [`get_all_commits_paths`].
pub fn get_all_commits_paths_in(cwd: &Path, paths: &[String]) -> Result<Vec<Commit>> {
    let mut args = vec![
        "-c".to_string(),
        "log.showSignature=false".to_string(),
        "log".to_string(),
        "--pretty=format:%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e".to_string(),
        "HEAD".to_string(),
    ];
    if !paths.is_empty() {
        args.push("--".to_string());
        for p in paths {
            args.push(p.clone());
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = git_output_in(cwd, &arg_refs)?;
    Ok(parse_commit_output(&output))
}

/// A commit paired with the workspace-relative paths it touched.
///
/// Produced by the `--name-only` fetch variants so the changelog renderers can
/// apply a precise `changelog.paths` glob intersect over the git-pathspec
/// scope (see [`crate::changelog_scope`]).
#[derive(Debug, Clone)]
pub struct CommitWithFiles {
    /// The commit metadata.
    pub commit: Commit,
    /// Paths this commit touched, relative to the repo root.
    pub files: Vec<String>,
}

/// Parse `git log --name-only` output (metadata formatted as
/// `%H%x1f...%b%x1e`, followed by one touched-file path per line) into
/// [`CommitWithFiles`].
///
/// git emits each commit as `<metadata>\x1e\n<file>\n<file>\n\n` (the touched
/// files follow the `%x1e`-terminated metadata, then a blank line). Splitting
/// on `\x1e` yields `[metadata_0, "\n<files_0>\n\n<metadata_1>", ...]`: the
/// file block trailing each record up to the next metadata belongs to THAT
/// record's commit.
///
/// The metadata record is multi-line because `%b` (the commit body) carries
/// newlines, so the record runs from the first `\x1f`-bearing line through the
/// end of the segment — NOT just the first matching line. Truncating to one
/// line would drop body trailers (e.g. `Co-Authored-By:`) for every commit
/// after the first, diverging from the full-body parse the changelog stage's
/// `parse_git_log_records` performs.
pub fn parse_commit_output_with_files(output: &str) -> Vec<CommitWithFiles> {
    if output.is_empty() {
        return vec![];
    }
    let segments: Vec<&str> = output.split('\x1e').collect();
    let mut out: Vec<CommitWithFiles> = Vec::new();
    // segments[i] for i>0 begins with the file block of commit i-1 followed by
    // the metadata of commit i. The first segment is pure metadata (commit 0);
    // the last segment is the file block of the final commit (no trailing
    // metadata). Walk pairwise: metadata from this segment, files from the
    // NEXT segment's leading lines (before its own metadata's first field).
    for (idx, seg) in segments.iter().enumerate() {
        // The metadata of commit `idx` is the part of `seg` AFTER the leading
        // file block (file block present only for idx>0). For idx==0 the whole
        // segment is metadata. For idx>0 the metadata record begins at the
        // first `\x1f`-bearing line and continues to the segment end (a
        // multi-line `%b` body keeps emitting newline-separated lines after the
        // unit-separator fields), so the remainder is kept verbatim — joined
        // from that line onward — rather than just the first matching line.
        let metadata = if idx == 0 {
            seg.trim_start_matches(['\n', '\r']).to_string()
        } else {
            let lines: Vec<&str> = seg.split('\n').collect();
            match lines.iter().position(|line| line.contains('\x1f')) {
                Some(start) => lines[start..].join("\n"),
                None => String::new(),
            }
        };
        if metadata.trim().is_empty() {
            continue;
        }
        let commits = parse_commit_output(&metadata);
        let Some(commit) = commits.into_iter().next() else {
            continue;
        };
        // Files for THIS commit are the leading lines of the NEXT segment,
        // before that segment's own metadata line.
        let files = match segments.get(idx + 1) {
            Some(next) => next
                .split('\n')
                .map(str::trim)
                .take_while(|line| !line.contains('\x1f'))
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect(),
            None => Vec::new(),
        };
        out.push(CommitWithFiles { commit, files });
    }
    out
}

/// `--name-only` sibling of [`get_commits_between_paths_in`]: each commit is
/// paired with the repo-relative paths it touched, for a precise
/// `changelog.paths` glob intersect over the git-pathspec scope.
pub fn get_commits_between_paths_with_files_in(
    cwd: &Path,
    from: &str,
    to: &str,
    paths: &[String],
) -> Result<Vec<CommitWithFiles>> {
    let range = format!("{}..{}", from, to);
    let mut args = vec![
        "-c".to_string(),
        "log.showSignature=false".to_string(),
        "log".to_string(),
        "--name-only".to_string(),
        "--pretty=format:%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e".to_string(),
        range,
    ];
    if !paths.is_empty() {
        args.push("--".to_string());
        for p in paths {
            args.push(p.clone());
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = git_output_in(cwd, &arg_refs)?;
    Ok(parse_commit_output_with_files(&output))
}

/// `--name-only` sibling of [`get_all_commits_paths_in`].
pub fn get_all_commits_paths_with_files_in(
    cwd: &Path,
    paths: &[String],
) -> Result<Vec<CommitWithFiles>> {
    let mut args = vec![
        "-c".to_string(),
        "log.showSignature=false".to_string(),
        "log".to_string(),
        "--name-only".to_string(),
        "--pretty=format:%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e".to_string(),
        "HEAD".to_string(),
    ];
    if !paths.is_empty() {
        args.push("--".to_string());
        for p in paths {
            args.push(p.clone());
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = git_output_in(cwd, &arg_refs)?;
    Ok(parse_commit_output_with_files(&output))
}

/// All commits reachable from an arbitrary `rev` (not just `HEAD`), filtered to
/// `paths`. Used by the changelog stage to bound a no-lower-bound range at an
/// explicit upper ref (`changelog ..<tag>`): the range is then every ancestor
/// of `<tag>`, excluding commits made after it.
pub fn get_commits_reachable_paths_in(
    cwd: &Path,
    rev: &str,
    paths: &[String],
) -> Result<Vec<Commit>> {
    let mut args = vec![
        "-c".to_string(),
        "log.showSignature=false".to_string(),
        "log".to_string(),
        "--pretty=format:%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e".to_string(),
        rev.to_string(),
    ];
    if !paths.is_empty() {
        args.push("--".to_string());
        for p in paths {
            args.push(p.clone());
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = git_output_in(cwd, &arg_refs)?;
    Ok(parse_commit_output(&output))
}

/// `--name-only` sibling of [`get_commits_reachable_paths_in`].
pub fn get_commits_reachable_paths_with_files_in(
    cwd: &Path,
    rev: &str,
    paths: &[String],
) -> Result<Vec<CommitWithFiles>> {
    let mut args = vec![
        "-c".to_string(),
        "log.showSignature=false".to_string(),
        "log".to_string(),
        "--name-only".to_string(),
        "--pretty=format:%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e".to_string(),
        rev.to_string(),
    ];
    if !paths.is_empty() {
        args.push("--".to_string());
        for p in paths {
            args.push(p.clone());
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = git_output_in(cwd, &arg_refs)?;
    Ok(parse_commit_output_with_files(&output))
}

/// Get last N commit subjects.
pub fn get_last_commit_messages(count: usize) -> Result<Vec<String>> {
    get_last_commit_messages_in(&cwd_or_dot(), count)
}

/// Path-taking sibling of [`get_last_commit_messages`].
pub fn get_last_commit_messages_in(cwd: &Path, count: usize) -> Result<Vec<String>> {
    let output = git_output_in(
        cwd,
        &[
            "-c",
            "log.showSignature=false",
            "log",
            &format!("-{count}"),
            "--pretty=format:%s",
        ],
    )?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Get commit subjects between two refs.
pub fn get_commit_messages_between(from: &str, to: &str) -> Result<Vec<String>> {
    get_commit_messages_between_in(&cwd_or_dot(), from, to)
}

/// Path-taking sibling of [`get_commit_messages_between`].
pub fn get_commit_messages_between_in(cwd: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let output = git_output_in(
        cwd,
        &[
            "-c",
            "log.showSignature=false",
            "log",
            "--pretty=format:%s",
            &format!("{from}..{to}"),
        ],
    )?;
    Ok(output.lines().map(str::to_string).collect())
}
