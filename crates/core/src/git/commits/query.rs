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

/// `git log` format for a commit record: six NUL-terminated fields (hash,
/// short hash, subject, author name, author email, body).
///
/// NUL is the one byte git refuses in a commit message and in a path, so no
/// field can contain the separator. The record and unit separators this format
/// used to carry are ordinary bytes a message may hold, and a commit whose
/// message held one was silently dropped or shifted its neighbours' fields.
pub const COMMIT_RECORD_FORMAT: &str = "--pretty=format:%H%x00%h%x00%s%x00%an%x00%ae%x00%b%x00";
const COMMIT_RECORD_FIELDS: usize = 6;

/// Split [`COMMIT_RECORD_FORMAT`] output into records. Each record carries the
/// text git printed before its hash (the newline between commits, or a
/// `--name-only` file block) and its six fields; the text after the last
/// record (the last commit's file block) is returned beside them. An
/// incomplete trailing record is dropped.
fn split_commit_records(output: &str) -> (Vec<(&str, [&str; COMMIT_RECORD_FIELDS])>, &str) {
    let tokens: Vec<&str> = output.split('\0').collect();
    let (chunks, rest) = tokens.as_chunks::<COMMIT_RECORD_FIELDS>();
    let trailing = rest.first().copied().unwrap_or("");
    let records = chunks
        .iter()
        .map(|fields| {
            let (leading, hash) = fields[0].rsplit_once('\n').unwrap_or(("", fields[0]));
            (
                leading,
                [
                    hash.trim(),
                    fields[1],
                    fields[2],
                    fields[3],
                    fields[4],
                    fields[5],
                ],
            )
        })
        .collect();
    (records, trailing)
}

fn commit_from_fields(fields: &[&str; COMMIT_RECORD_FIELDS]) -> Option<Commit> {
    if fields[0].is_empty() {
        return None;
    }
    Some(Commit {
        hash: fields[0].to_string(),
        short_hash: fields[1].to_string(),
        message: fields[2].to_string(),
        author_name: fields[3].to_string(),
        author_email: fields[4].to_string(),
        body: fields[5].trim().to_string(),
    })
}

/// Parse [`COMMIT_RECORD_FORMAT`] git log output into a vec of [`Commit`]s.
///
/// The single record decoder for this wire format: every changelog path
/// (`parse_commit_output_with_files` here, and the changelog stage's git
/// fetch) decodes through this function so the body / author fields can never
/// drift between call sites.
pub fn parse_commit_output(output: &str) -> Vec<Commit> {
    split_commit_records(output)
        .0
        .iter()
        .filter_map(|(_, fields)| commit_from_fields(fields))
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
        COMMIT_RECORD_FORMAT.to_string(),
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
        COMMIT_RECORD_FORMAT.to_string(),
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

/// Parse `git log --name-only` output in [`COMMIT_RECORD_FORMAT`] into
/// [`CommitWithFiles`].
///
/// git prints each commit's touched files after its record, one path per
/// line, so a commit's files are the text between its last field and the next
/// commit's hash, and the last commit's files are the text after the stream's
/// last record.
pub fn parse_commit_output_with_files(output: &str) -> Vec<CommitWithFiles> {
    let (records, trailing) = split_commit_records(output);
    let mut out = Vec::with_capacity(records.len());
    for (idx, (_, fields)) in records.iter().enumerate() {
        let Some(commit) = commit_from_fields(fields) else {
            continue;
        };
        let files_text = records
            .get(idx + 1)
            .map_or(trailing, |(leading, _)| *leading);
        let files = files_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
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
        COMMIT_RECORD_FORMAT.to_string(),
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
        COMMIT_RECORD_FORMAT.to_string(),
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
        COMMIT_RECORD_FORMAT.to_string(),
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
        COMMIT_RECORD_FORMAT.to_string(),
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

/// Get the last N commit messages as release signals: each commit, then,
/// under `expand_squash_merges`, the subjects a squash merge listed in its
/// body.
pub fn get_last_commit_messages(count: usize, expand_squash_merges: bool) -> Result<Vec<String>> {
    get_last_commit_messages_in(&cwd_or_dot(), count, expand_squash_merges)
}

/// Path-taking sibling of [`get_last_commit_messages`].
pub fn get_last_commit_messages_in(
    cwd: &Path,
    count: usize,
    expand_squash_merges: bool,
) -> Result<Vec<String>> {
    let output = git_output_in(
        cwd,
        &[
            "-c",
            "log.showSignature=false",
            "log",
            &format!("-{count}"),
            RELEASE_MESSAGE_FORMAT,
        ],
    )?;
    Ok(release_messages(&output, expand_squash_merges))
}

/// `git log` format for a release-signal read: subject and body, each
/// NUL-terminated (see [`COMMIT_RECORD_FORMAT`] for why NUL).
pub(crate) const RELEASE_MESSAGE_FORMAT: &str = "--pretty=format:%s%x00%b%x00";

/// Decode [`RELEASE_MESSAGE_FORMAT`] output into the messages the auto-bump
/// reads: each commit's own message followed, under `expand_squash_merges`,
/// by its squashed subjects.
pub(crate) fn release_messages(output: &str, expand_squash_merges: bool) -> Vec<String> {
    let tokens: Vec<&str> = output.split('\0').collect();
    tokens
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[subject, body]| (subject.trim(), body.trim()))
        .filter(|(subject, body)| !(subject.is_empty() && body.is_empty()))
        .flat_map(|(subject, body)| messages_with_squashed(subject, body, expand_squash_merges))
        .collect()
}

/// Get the commit messages between two refs as release signals (see
/// [`get_last_commit_messages`]).
pub fn get_commit_messages_between(
    from: &str,
    to: &str,
    expand_squash_merges: bool,
) -> Result<Vec<String>> {
    get_commit_messages_between_in(&cwd_or_dot(), from, to, expand_squash_merges)
}

/// Path-taking sibling of [`get_commit_messages_between`].
pub fn get_commit_messages_between_in(
    cwd: &Path,
    from: &str,
    to: &str,
    expand_squash_merges: bool,
) -> Result<Vec<String>> {
    let output = git_output_in(
        cwd,
        &[
            "-c",
            "log.showSignature=false",
            "log",
            RELEASE_MESSAGE_FORMAT,
            &format!("{from}..{to}"),
        ],
    )?;
    Ok(release_messages(&output, expand_squash_merges))
}
