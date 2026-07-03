use anyhow::{Context as _, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::git_output_in;

/// Subject prefix anodizer stamps on its own release-machinery commits
/// (version-sync bumps, rollback reverts). The matchers that must recognise
/// those commits — rollback's idempotency check, the changelog stage's
/// version-sync exclusion — compose their patterns from this same constant
/// so a reworded writer can never silently break a matcher.
pub const RELEASE_COMMIT_PREFIX: &str = "chore(release): ";

/// `chore(release): bump ` — the subject prefix shared by every version-sync
/// bump commit (see [`release_bump_subject`]).
pub fn release_bump_subject_prefix() -> String {
    format!("{RELEASE_COMMIT_PREFIX}bump ")
}

/// Build a version-sync bump commit subject:
/// `chore(release): bump <summary><suffix>`. `suffix` carries the optional
/// ` [skip ci]` marker (empty when none applies).
pub fn release_bump_subject(summary: &str, suffix: &str) -> String {
    format!("{}{summary}{suffix}", release_bump_subject_prefix())
}

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

fn cwd_or_dot() -> PathBuf {
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

/// Get the current branch name.
pub fn get_current_branch() -> Result<String> {
    get_current_branch_in(&cwd_or_dot())
}

/// Return `true` when `name` looks like a branch (NOT an anodize-shaped
/// release tag). Tag shapes: `^v\d+\.\d+\.\d+` (lockstep
/// `v1.2.3[-pre][+build]`) or `^<crate>-v\d+\.\d+\.\d+`
/// (per-crate `mycrate-v1.2.3[...]`).
///
/// Both regexes are start-anchored, and the per-crate `<crate>` segment is
/// constrained to non-`/` characters. Without that, a branch like
/// `feature/fix-v2.0.0` contains `-v2.0.0` and would be misclassified as a
/// tag — leaving its `GITHUB_REF_NAME` fallback rejected. A real per-crate
/// tag's name prefix is a crate name (no path separators), so anchoring on
/// `^[^/]+-v` keeps that branch shape branch-like while still matching
/// `mycrate-v1.2.3`.
///
/// Guards the `GITHUB_REF_NAME` fallback in [`get_current_branch_in`]: on
/// a `push: tags:` workflow trigger, `GITHUB_REF_NAME` is the TAG name
/// (e.g. `v0.4.5`), and accepting it would make `git push origin v0.4.5`
/// from detached HEAD silently create a branch named after the tag.
///
/// Drift-risk pair with `cli::commands::tag::rollback`'s `LOCKSTEP_TAG_RE` /
/// `PER_CRATE_TAG_RE`: those classify the same two anodize tag shapes but
/// are fully anchored and strict (a rollback must touch only real tags).
/// The patterns here are deliberately looser and prefix-only (branch-vs-tag
/// disambiguation, not classification). Keep both in sync when the tag
/// grammar changes — they are intentionally separate, not duplicated.
pub fn is_branchlike(name: &str) -> bool {
    use regex::Regex;
    use std::sync::OnceLock;
    static LOCKSTEP: OnceLock<Regex> = OnceLock::new();
    static PER_CRATE: OnceLock<Regex> = OnceLock::new();
    let lockstep = LOCKSTEP.get_or_init(|| Regex::new(r"^v\d+\.\d+\.\d+").expect("static regex"));
    let per_crate =
        PER_CRATE.get_or_init(|| Regex::new(r"^[^/]+-v\d+\.\d+\.\d+").expect("static regex"));
    !(lockstep.is_match(name) || per_crate.is_match(name))
}

/// Path-taking sibling of [`get_current_branch`].
///
/// Handles detached-HEAD checkouts (e.g. `actions/checkout@v4` with `ref:`)
/// by resolving the branch HEAD points at via `for-each-ref`, falling back
/// to the remote's default branch and finally `GITHUB_REF_NAME` when set —
/// so downstream `git push origin <branch>` produces a valid refspec
/// instead of a literal `HEAD` that git can't auto-qualify.
///
/// The `GITHUB_REF_NAME` fallback is guarded by [`is_branchlike`]: on a
/// `push: tags:` trigger, `GITHUB_REF_NAME` is the TAG name, and accepting
/// it would push to a branch named after the tag. Tag-shaped values fall
/// through to the bail at the end so callers hard-fail and prompt the
/// operator for `--branch <name>` explicitly.
pub fn get_current_branch_in(cwd: &Path) -> Result<String> {
    get_current_branch_in_with_env(cwd, &crate::ProcessEnvSource)
}

/// [`EnvSource`](crate::EnvSource)-injecting form of [`get_current_branch_in`].
///
/// Reads the `GITHUB_REF_NAME` fallback from `env` rather than the process
/// environment, so tests can drive the tag-shaped / branch-shaped fallback
/// branches deterministically without mutating global env state.
pub fn get_current_branch_in_with_env<E: crate::EnvSource + ?Sized>(
    cwd: &Path,
    env: &E,
) -> Result<String> {
    if let Ok(name) = git_output_in(cwd, &["symbolic-ref", "--short", "HEAD"]) {
        return Ok(name);
    }
    if let Ok(out) = git_output_in(
        cwd,
        &[
            "for-each-ref",
            "--points-at",
            "HEAD",
            "--format=%(refname:short)",
            "refs/heads/",
        ],
    ) && !out.is_empty()
    {
        let branches: Vec<&str> = out.lines().collect();
        for preferred in ["master", "main"] {
            if branches.contains(&preferred) {
                return Ok(preferred.to_string());
            }
        }
        if let Some(first) = branches.first() {
            return Ok((*first).to_string());
        }
    }
    if let Ok(out) = git_output_in(
        cwd,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) && let Some(name) = out.strip_prefix("origin/")
    {
        return Ok(name.to_string());
    }
    if let Some(name) = env.var("GITHUB_REF_NAME")
        && !name.is_empty()
        && is_branchlike(&name)
    {
        return Ok(name);
    }
    anyhow::bail!(
        "could not resolve current branch: HEAD is detached and no fallback (points-at-HEAD branches, origin/HEAD, GITHUB_REF_NAME) succeeded"
    )
}

/// Return remote branch short names that contain `sha` (e.g. `master`,
/// `release/v1`). The bump commit's SHA is the deterministic anchor of
/// the tag, so deriving the push branch from it is race-immune to the
/// default branch moving between bump and rollback. Empty `Vec` when
/// the SHA is not on any remote branch (orphan / not-yet-pushed).
pub fn branches_containing_sha_in(cwd: &Path, sha: &str) -> Result<Vec<String>> {
    let out = git_output_in(
        cwd,
        &[
            "branch",
            "-r",
            "--contains",
            sha,
            "--format=%(refname:short)",
        ],
    )?;
    Ok(out
        .lines()
        .filter_map(|line| line.trim().strip_prefix("origin/").map(str::to_string))
        .filter(|name| !name.is_empty() && name != "HEAD")
        .collect())
}

/// Check if there are any commits since a given tag.
pub fn has_commits_since_tag(tag: &str) -> Result<bool> {
    has_commits_since_tag_in(&cwd_or_dot(), tag)
}

/// Path-taking sibling of [`has_commits_since_tag`].
pub fn has_commits_since_tag_in(cwd: &Path, tag: &str) -> Result<bool> {
    let range = format!("{}..HEAD", tag);
    let output = git_output_in(
        cwd,
        &["-c", "log.showSignature=false", "log", "--oneline", &range],
    )?;
    Ok(!output.is_empty())
}

/// Count the commits on HEAD since the most recent reachable tag.
///
/// Resolves the last tag with `git describe --tags --abbrev=0 HEAD`, then
/// returns `git rev-list --count <tag>..HEAD`. When HEAD has no reachable
/// tag (a repo whose first version tag has not landed yet), the total
/// commit count on HEAD is returned instead (`git rev-list --count HEAD`).
///
/// `monorepo_prefix` constrains the `describe` to tags matching
/// `<prefix>*` (via `--match`), so in a per-crate workspace the count is
/// since the matching crate's tag rather than the nearest tag from ANY
/// subproject. `None` considers all tags.
///
/// This is the stateless basis for the `{{ .NightlyBuild }}` template var:
/// the count resets to a small number the moment a new version tag lands,
/// so a nightly build counter increments per base version with no state
/// anodizer must persist.
///
/// Returns `Ok(0)` for an empty repository (no commits) so callers never
/// have to special-case the unborn-HEAD state.
pub fn count_commits_since_last_tag_in(cwd: &Path, monorepo_prefix: Option<&str>) -> Result<u64> {
    // `--abbrev=0` yields the bare tag name (no `-<n>-g<sha>` suffix).
    // A repo with no reachable tag exits non-zero here; treat that as
    // "count every commit on HEAD" rather than an error.
    //
    // `--match=<prefix>*` (when a monorepo prefix is set) restricts the
    // describe to the matching crate's tags — without it, describe returns
    // the nearest reachable tag from ANY subproject and the count would be
    // since the wrong crate's tag. Mirrors `find_previous_tag_with_prefix_in`.
    let match_arg;
    let mut describe_args: Vec<&str> = vec!["describe", "--tags", "--abbrev=0"];
    if let Some(prefix) = monorepo_prefix {
        match_arg = format!("--match={}*", prefix);
        describe_args.push(&match_arg);
    }
    describe_args.push("HEAD");
    let range = match git_output_in(cwd, &describe_args) {
        Ok(tag) if !tag.is_empty() => format!("{tag}..HEAD"),
        _ => "HEAD".to_string(),
    };
    // An empty repo (unborn HEAD) makes `rev-list` fail; map that to 0.
    let count = match git_output_in(cwd, &["rev-list", "--count", &range]) {
        Ok(s) => s.trim().parse::<u64>().unwrap_or(0),
        Err(_) => 0,
    };
    Ok(count)
}

/// Get the short commit hash of HEAD.
pub fn get_short_commit() -> Result<String> {
    get_short_commit_in(&cwd_or_dot())
}

/// Path-taking sibling of [`get_short_commit`].
pub fn get_short_commit_in(cwd: &Path) -> Result<String> {
    git_output_in(cwd, &["rev-parse", "--short", "HEAD"])
}

/// Default short-commit length used across error messages, log
/// output, and any place that needs to truncate a full SHA for
/// human display. Matches git's `--short` default (7) — and the
/// `ShortCommit` template var populated by [`super::detect_git_info`]
/// (which delegates to `git rev-parse --short`).
pub const SHORT_COMMIT_LEN: usize = 7;

/// Truncate a full commit SHA string to [`SHORT_COMMIT_LEN`]
/// characters. Returns the input unchanged when it's already shorter
/// or equal in length. Use this any time the SHA arrives as a string
/// (e.g. deserialized from a manifest or read from a template var)
/// rather than running `git rev-parse --short` again — saves a
/// subprocess and keeps the length convention in one place.
///
/// Empty input returns empty; callers needing fail-closed semantics
/// (e.g. publish-only's commit cross-check) check `is_empty()`
/// before calling.
pub fn short_commit_str(commit: &str) -> String {
    if commit.len() > SHORT_COMMIT_LEN {
        commit[..SHORT_COMMIT_LEN].to_string()
    } else {
        commit.to_string()
    }
}

/// Get the full commit hash of HEAD.
///
/// The full commit SHA (resolved at git-pipe time and
/// reused everywhere downstream). Used by the source-archive stage to
/// produce deterministic archives across consecutive commits when
/// `git_info` was not pre-populated by an earlier pipe.
pub fn get_head_commit() -> Result<String> {
    get_head_commit_in(&cwd_or_dot())
}

/// Path-taking sibling of [`get_head_commit`].
pub fn get_head_commit_in(cwd: &Path) -> Result<String> {
    git_output_in(cwd, &["rev-parse", "HEAD"])
}

/// Check if there are changes in a path since a given tag.
pub fn has_changes_since(tag: &str, path: &str) -> Result<bool> {
    has_changes_since_in(&cwd_or_dot(), tag, path)
}

/// Path-taking sibling of [`has_changes_since`].
pub fn has_changes_since_in(cwd: &Path, tag: &str, path: &str) -> Result<bool> {
    let output = git_output_in(
        cwd,
        &["diff", "--name-only", &format!("{}..HEAD", tag), "--", path],
    )?;
    Ok(!output.is_empty())
}

/// Get last N commit subjects that touched a specific path.
pub fn get_last_commit_messages_path(count: usize, path: &str) -> Result<Vec<String>> {
    get_last_commit_messages_path_in(&cwd_or_dot(), count, path)
}

/// Path-taking sibling of [`get_last_commit_messages_path`].
pub fn get_last_commit_messages_path_in(
    cwd: &Path,
    count: usize,
    path: &str,
) -> Result<Vec<String>> {
    let output = git_output_in(
        cwd,
        &[
            "-c",
            "log.showSignature=false",
            "log",
            &format!("-{count}"),
            "--pretty=format:%s",
            "--",
            path,
        ],
    )?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Get commit subjects between two refs that touched a specific path.
pub fn get_commit_messages_between_path(from: &str, to: &str, path: &str) -> Result<Vec<String>> {
    get_commit_messages_between_path_in(&cwd_or_dot(), from, to, path)
}

/// Path-taking sibling of [`get_commit_messages_between_path`].
pub fn get_commit_messages_between_path_in(
    cwd: &Path,
    from: &str,
    to: &str,
    path: &str,
) -> Result<Vec<String>> {
    let output = git_output_in(
        cwd,
        &[
            "-c",
            "log.showSignature=false",
            "log",
            "--pretty=format:%s",
            &format!("{from}..{to}"),
            "--",
            path,
        ],
    )?;
    Ok(output.lines().map(str::to_string).collect())
}

/// Stage specific files and create a commit.
///
/// Returns `Ok(true)` when a commit was created, `Ok(false)` when staging
/// produced no diff (e.g. files are already at the target state) — callers
/// that need idempotent bump-then-tag flows can use the boolean to decide
/// whether to skip downstream commit-dependent work without inspecting git
/// state separately.
pub fn stage_and_commit(files: &[&str], message: &str) -> Result<bool> {
    stage_and_commit_in(&cwd_or_dot(), files, message)
}

/// Path-taking sibling of [`stage_and_commit`].
pub fn stage_and_commit_in(cwd: &Path, files: &[&str], message: &str) -> Result<bool> {
    let mut args = vec!["add", "--"];
    args.extend(files.iter().copied());
    git_output_in(cwd, &args)?;
    // Idempotency guard: `git add` happily stages nothing when the working
    // tree already matches HEAD for the given paths. Running `git commit`
    // after would fail with "nothing to commit" (printed to stdout, not
    // stderr) and surface a confusing empty-stderr error. Detect the
    // no-diff case here so callers can re-run safely.
    let diff = Command::new("git")
        .current_dir(cwd)
        .args(["diff", "--cached", "--quiet", "--"])
        .args(files)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .status()?;
    if diff.success() {
        return Ok(false);
    }
    git_output_in(cwd, &["commit", "-m", message])?;
    Ok(true)
}

/// `git -C <workspace_root> -c log.showSignature=false log
/// --pretty=format:%B%x1e <range> -- <rel_path>` — list commit message
/// bodies (subject+body) for commits in `range` touching `rel_path`,
/// using the `\x1e` (RS) byte as a between-commits separator so multi-line
/// bodies survive parsing.
///
/// `range` is the git revision range as a string (e.g. `"HEAD"`,
/// `"v0.3.0..HEAD"`); the empty string is invalid (caller must pre-filter).
/// Returns `Ok(Vec::new())` when the range's base does not exist yet
/// (unknown/bad revision, empty repo) — a legitimate "no commits" outcome.
/// Any other git failure (e.g. an invalid pathspec) is an `Err`, never an
/// empty success.
pub fn log_subjects_for_range(
    workspace_root: &std::path::Path,
    range: &str,
    rel_path: &str,
) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args([
            "-c",
            "log.showSignature=false",
            "log",
            "--pretty=format:%B%x1e",
            range,
            "--",
            rel_path,
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()?;
    if !out.status.success() {
        // A range whose base doesn't exist yet (no prior tag, empty repo)
        // is a legitimate "no commits" outcome. Any other fatal (e.g. an
        // empty pathspec) must propagate — swallowing it made `bump`
        // preview Skip on repos whose crate lives at the workspace root.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let missing_range = stderr.contains("unknown revision")
            || stderr.contains("bad revision")
            || stderr.contains("does not have any commits yet");
        if missing_range {
            return Ok(Vec::new());
        }
        let raw = format!("git log {} failed: {}", range, stderr.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .split('\x1e')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// `git -C <workspace_root> add <rel>` — stage a single relative path.
pub fn add_path_in(workspace_root: &std::path::Path, rel: &std::path::Path) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .arg("add")
        .arg(rel)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .context("failed to invoke git add")?;
    if !out.status.success() {
        let stderr_raw = String::from_utf8_lossy(&out.stderr);
        let raw = format!("git add {} failed: {}", rel.display(), stderr_raw.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    Ok(())
}

/// `git -C <workspace_root> commit [-S] -m <message>` — create a commit
/// with the given message, optionally GPG-signed.
pub fn commit_in(workspace_root: &std::path::Path, message: &str, sign: bool) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(workspace_root).arg("commit");
    if sign {
        cmd.arg("-S");
    }
    cmd.arg("-m")
        .arg(message)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    let out = cmd.output().context("failed to invoke git commit")?;
    if !out.status.success() {
        let stderr_raw = String::from_utf8_lossy(&out.stderr);
        let raw = format!("git commit failed: {}", stderr_raw.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    Ok(())
}

/// `git diff --name-only <tag>..HEAD -- <paths>...` — return `true` when
/// any of the named paths changed between `tag` and `HEAD`. Returns
/// `Ok(false)` when git fails (e.g. not a git repo) so callers can treat
/// the absence-of-info case as "no changes".
pub fn paths_changed_since_tag(tag: &str, paths: &[&str]) -> Result<bool> {
    paths_changed_since_tag_in(&cwd_or_dot(), tag, paths)
}

/// Path-taking sibling of [`paths_changed_since_tag`].
pub fn paths_changed_since_tag_in(cwd: &Path, tag: &str, paths: &[&str]) -> Result<bool> {
    let mut args: Vec<String> = vec![
        "diff".to_string(),
        "--name-only".to_string(),
        format!("{tag}..HEAD"),
        "--".to_string(),
    ];
    for p in paths {
        args.push((*p).to_string());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = Command::new("git")
        .current_dir(cwd)
        .args(&arg_refs)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()?;
    if output.status.success() {
        Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
    } else {
        Ok(false)
    }
}

/// Return HEAD's full commit hash for the given repository (or worktree).
///
/// Retained as a named entry point for the determinism harness / CI glue;
/// delegates to [`get_head_commit_in`] so HEAD-sha resolution lives in exactly
/// one place rather than re-implementing its own `rev-parse`.
pub fn head_commit_hash_in(repo: &std::path::Path) -> Result<String> {
    get_head_commit_in(repo)
}

/// Resolve a revision (sha, ref name, `HEAD`, etc.) to its full commit hash.
///
/// Wrapper over `git rev-parse <rev>` — errors when the revision can't be
/// resolved (unknown sha, ambiguous short hash, not a git repo).
pub fn rev_parse_in(cwd: &Path, rev: &str) -> Result<String> {
    git_output_in(cwd, &["rev-parse", rev])
}

/// `git rev-parse --verify <rev>^{commit}` — resolve `rev` to a commit SHA,
/// erroring when it does not name an existing commit. Stricter than
/// [`rev_parse_in`]: `--verify` rejects ambiguous / non-existent refs (rather
/// than echoing the input back), and the `^{commit}` peel rejects a ref that
/// resolves to a non-commit object (e.g. a tree or blob SHA).
pub fn rev_verify_commit_in(cwd: &Path, rev: &str) -> Result<String> {
    git_output_in(
        cwd,
        &["rev-parse", "--verify", &format!("{}^{{commit}}", rev)],
    )
}

/// `git rev-list <sha>..HEAD` — list the commit hashes (newest-first) that
/// sit on top of `sha` but aren't in `sha`.
///
/// Returns an empty vec when `sha` IS `HEAD` (no commits between).
pub fn commits_between_in(cwd: &Path, sha: &str) -> Result<Vec<String>> {
    let range = format!("{}..HEAD", sha);
    let out = git_output_in(cwd, &["rev-list", &range])?;
    if out.is_empty() {
        return Ok(Vec::new());
    }
    Ok(out.lines().map(|s| s.trim().to_string()).collect())
}

/// `git log -1 --format=%s <sha>` — return the subject line of a single
/// commit. Used to render the "non-bump commit subject" list when the
/// rollback safety check fires.
pub fn commit_subject_in(cwd: &Path, sha: &str) -> Result<String> {
    git_output_in(
        cwd,
        &[
            "-c",
            "log.showSignature=false",
            "log",
            "-1",
            "--format=%s",
            sha,
        ],
    )
}

/// `git log --format=%H%x1f%s <sha>..HEAD` — return every `(full_sha, subject)`
/// pair in the range in one subprocess. Used by the rollback safety check so
/// classifying N intervening commits is a single `git` spawn rather than
/// `1 + N` (one `rev-list` plus one `log -1` per commit).
///
/// Empty range (sha IS HEAD) returns an empty vec.
pub fn commits_with_subjects_in(cwd: &Path, sha: &str) -> Result<Vec<(String, String)>> {
    let range = format!("{}..HEAD", sha);
    let out = git_output_in(
        cwd,
        &[
            "-c",
            "log.showSignature=false",
            "log",
            "--format=%H%x1f%s",
            &range,
        ],
    )?;
    if out.is_empty() {
        return Ok(Vec::new());
    }
    Ok(out
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '\x1f');
            let sha = parts.next()?.trim().to_string();
            let subj = parts.next().unwrap_or("").to_string();
            if sha.is_empty() {
                None
            } else {
                Some((sha, subj))
            }
        })
        .collect())
}

/// Committer identity (author + committer name/email) for the rare path
/// where a git invocation lands on a host with no `user.email` /
/// `user.name` configured — notably `actions/checkout@v6`, which does
/// NOT set committer identity for the workflow runner. Resolved once per
/// caller and threaded through to [`revert_commit_in`] so the CLI never
/// mutates the repo's git config (env-only, scoped to the single spawn).
///
/// Convention: when both `name` and `email` are populated, the values
/// are exported as `GIT_AUTHOR_NAME` / `GIT_AUTHOR_EMAIL` AND
/// `GIT_COMMITTER_NAME` / `GIT_COMMITTER_EMAIL` on the git child
/// processes (revert + amend). When `None`, the child inherits whatever
/// the parent / repo config provides.
#[derive(Debug, Clone, Default)]
pub struct CommitterIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl CommitterIdentity {
    /// Return a default committer identity to use when `user.email` and
    /// `user.name` are both unset on the host. Email uses the
    /// short-hostname (best-effort; falls back to `"localhost"`) so a
    /// reviewer can tell at a glance which machine emitted the
    /// rollback commit.
    pub fn default_for_rollback() -> Self {
        let host = std::env::var("HOSTNAME")
            .ok()
            .or_else(|| std::env::var("COMPUTERNAME").ok())
            .and_then(|h| h.split('.').next().map(str::to_string))
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        Self {
            name: Some("anodize-rollback".to_string()),
            email: Some(format!("anodize-rollback@{host}")),
        }
    }

    fn apply_to(&self, cmd: &mut Command) {
        if let Some(n) = &self.name {
            cmd.env("GIT_AUTHOR_NAME", n).env("GIT_COMMITTER_NAME", n);
        }
        if let Some(e) = &self.email {
            cmd.env("GIT_AUTHOR_EMAIL", e).env("GIT_COMMITTER_EMAIL", e);
        }
    }
}

/// Read `git config user.email` / `user.name` in `cwd`. Returns
/// `(name, email)`, each `Some(value)` when configured (and non-empty)
/// or `None` when unset. Used by [`revert_commit_in`] to detect the
/// CI-checkout case where neither identity is configured and the
/// committer env fallback must fire.
fn read_git_identity(cwd: &Path) -> (Option<String>, Option<String>) {
    let one = |key: &str| -> Option<String> {
        let out = Command::new("git")
            .current_dir(cwd)
            .args(["config", "--get", key])
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if value.is_empty() { None } else { Some(value) }
    };
    (one("user.name"), one("user.email"))
}

/// Resolve the committer identity to use for a rollback-style commit.
/// When the host already has `user.name` AND `user.email` configured
/// (or `GIT_AUTHOR_*` / `GIT_COMMITTER_*` are set in the parent env),
/// returns an empty identity so the child inherits the existing
/// values. Otherwise returns a synthetic identity so the commit
/// doesn't fail with "Author identity unknown" on bare-CI hosts.
pub fn resolve_rollback_identity(cwd: &Path) -> CommitterIdentity {
    let env_author_set =
        std::env::var("GIT_AUTHOR_EMAIL").is_ok() && std::env::var("GIT_AUTHOR_NAME").is_ok();
    let env_committer_set =
        std::env::var("GIT_COMMITTER_EMAIL").is_ok() && std::env::var("GIT_COMMITTER_NAME").is_ok();
    if env_author_set && env_committer_set {
        return CommitterIdentity::default();
    }
    let (name, email) = read_git_identity(cwd);
    if name.is_some() && email.is_some() {
        return CommitterIdentity::default();
    }
    CommitterIdentity::default_for_rollback()
}

/// Run `git revert --no-edit <sha>` in `cwd`, optionally followed by
/// `git commit --amend -m <message>`.
///
/// Refuses against a dirty working tree (`git revert` would surface a
/// less actionable "your local changes would be overwritten" message
/// otherwise). Mirrors the dirty-tree guard used by
/// `stage-publish/src/util/git_revert.rs`. The guard counts only
/// TRACKED modifications (`--untracked-files=no`): a revert never
/// touches untracked files, and a failure-recovery rollback runs right
/// after a release wrote `dist/` — in repos that don't gitignore their
/// dist, an untracked-counts-as-dirty guard would refuse every
/// post-release rollback. The one genuine hazard (an untracked file
/// where the revert must restore a tracked one) is refused by git
/// itself with an explicit "would be overwritten" error.
///
/// On revert failure (typically a merge conflict against later commits
/// on top of the bump), runs `git revert --abort` to restore the
/// working tree before bubbling the error — otherwise the next
/// rollback attempt would trip the dirty-tree guard and the operator
/// would be stuck.
///
/// `identity` is threaded through as committer env vars so the call
/// works on bare-CI hosts where the workflow checkout doesn't set
/// `user.email` / `user.name`. The env is scoped to the spawn; the
/// repo's git config is never mutated.
pub fn revert_commit_in(
    cwd: &Path,
    sha: &str,
    message: Option<&str>,
    identity: &CommitterIdentity,
) -> Result<()> {
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("revert_commit_in: git status in {}", cwd.display()))?;
    if !status.status.success() {
        let stderr_raw = String::from_utf8_lossy(&status.stderr);
        let raw = format!("git status failed: {}", stderr_raw.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    if !status.stdout.is_empty() {
        bail!(
            "refusing to revert in a dirty working tree at {}\nstatus:\n{}",
            cwd.display(),
            String::from_utf8_lossy(&status.stdout),
        );
    }

    let mut revert_cmd = Command::new("git");
    revert_cmd
        .current_dir(cwd)
        .args(["revert", "--no-edit", sha])
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0");
    identity.apply_to(&mut revert_cmd);
    let out = revert_cmd
        .output()
        .with_context(|| format!("revert_commit_in: git revert in {}", cwd.display()))?;
    if !out.status.success() {
        let stderr_raw = String::from_utf8_lossy(&out.stderr);
        // Restore the working tree before bubbling — otherwise the dirty-tree
        // guard above traps a subsequent rollback retry forever.
        let _ = Command::new("git")
            .current_dir(cwd)
            .args(["revert", "--abort"])
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output();
        let raw = format!(
            "git revert {sha} hit conflicts and was aborted (working tree restored). \
             The bump commit overlaps with later changes — resolve manually, \
             or re-run with --mode=reset to force.\nstderr: {}",
            stderr_raw.trim()
        );
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    if let Some(msg) = message {
        let mut amend_cmd = Command::new("git");
        amend_cmd
            .current_dir(cwd)
            .args(["commit", "--amend", "-m", msg])
            .env("LC_ALL", "C")
            .env("GIT_TERMINAL_PROMPT", "0");
        identity.apply_to(&mut amend_cmd);
        let out = amend_cmd.output().with_context(|| {
            format!("revert_commit_in: git commit --amend in {}", cwd.display())
        })?;
        if !out.status.success() {
            let stderr_raw = String::from_utf8_lossy(&out.stderr);
            let raw = format!("git commit --amend failed: {}", stderr_raw.trim());
            bail!("{}", crate::redact::redact_process_env(&raw));
        }
    }
    Ok(())
}

/// Run `git reset --hard <sha>` in `cwd`. **Destructive** — rewrites HEAD
/// and the index in place; callers must surface a warning before invoking.
pub fn reset_hard_in(cwd: &Path, sha: &str) -> Result<()> {
    git_output_in(cwd, &["reset", "--hard", sha])?;
    Ok(())
}

/// Push a branch (`HEAD:refs/heads/<branch>`) to the `origin` remote.
///
/// Errors when no `origin` remote is configured — callers driving local-only
/// flows should pass `--no-push` to skip the call entirely.
pub fn push_branch_in(cwd: &Path, branch: &str) -> Result<()> {
    if !super::has_remote_in(cwd, "origin") {
        bail!("no 'origin' remote configured, cannot push branch '{branch}'");
    }
    let refspec = format!("HEAD:refs/heads/{}", branch);
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["push", "origin", &refspec])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .with_context(|| format!("push_branch_in: git push origin {refspec}"))?;
    if !out.status.success() {
        let stderr_raw = String::from_utf8_lossy(&out.stderr);
        let raw = format!("git push origin {} failed: {}", refspec, stderr_raw.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    Ok(())
}

/// `git -C <repo> log -1 --format=%ct HEAD` — return HEAD's committer
/// timestamp (seconds since UNIX epoch) for the given repository. Used by
/// the determinism harness as the non-snapshot SDE seed.
pub fn head_commit_timestamp_in(repo: &std::path::Path) -> Result<i64> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "-1", "--format=%ct", "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .context("failed to invoke git log -1 --format=%ct HEAD")?;
    if !out.status.success() {
        let stderr_raw = String::from_utf8_lossy(&out.stderr);
        let raw = format!("git log -1 --format=%ct HEAD failed: {}", stderr_raw.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    text.parse::<i64>()
        .with_context(|| format!("git log --format=%ct returned non-i64 timestamp: {}", text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn init_repo_with_commits(dir: &Path, files: &[&str]) {
        let run = |args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            );
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["init"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "t"]);
        for (i, f) in files.iter().enumerate() {
            std::fs::write(dir.join(f), format!("c{i}")).unwrap();
            run(&["add", "."]);
            run(&["commit", "-m", &format!("commit-{i}: {f}")]);
        }
    }

    #[test]
    fn get_head_commit_in_returns_tempdirs_head_sha() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo_with_commits(tmp.path(), &["a"]);
        let expected = String::from_utf8(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["rev-parse", "HEAD"]).current_dir(tmp.path());
                    cmd
                },
                "git",
            )
            .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let sha = get_head_commit_in(tmp.path()).unwrap();
        assert_eq!(sha, expected);
    }

    #[test]
    fn get_short_commit_in_returns_tempdirs_short_sha() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo_with_commits(tmp.path(), &["a"]);
        let expected = String::from_utf8(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["rev-parse", "--short", "HEAD"])
                        .current_dir(tmp.path());
                    cmd
                },
                "git",
            )
            .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let short = get_short_commit_in(tmp.path()).unwrap();
        assert_eq!(short, expected);
    }

    #[test]
    fn has_commits_since_tag_in_returns_false_when_tag_is_head() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commits(dir, &["a"]);
        let run = |args: &[&str]| {
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            );
        };
        run(&["tag", "v1.0.0"]);
        assert!(!has_commits_since_tag_in(dir, "v1.0.0").unwrap());
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t.com")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t.com");
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
    }

    #[test]
    fn count_commits_since_last_tag_counts_commits_after_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // 2 commits, tag v1.0.0 at the 2nd, then 3 more commits.
        init_repo_with_commits(dir, &["a", "b"]);
        git_in(dir, &["tag", "v1.0.0"]);
        for f in ["c", "d", "e"] {
            std::fs::write(dir.join(f), "x").unwrap();
            git_in(dir, &["add", "."]);
            git_in(dir, &["commit", "-m", f]);
        }
        assert_eq!(count_commits_since_last_tag_in(dir, None).unwrap(), 3);
    }

    #[test]
    fn count_commits_since_last_tag_resets_on_newer_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commits(dir, &["a"]);
        git_in(dir, &["tag", "v1.0.0"]);
        for f in ["b", "c"] {
            std::fs::write(dir.join(f), "x").unwrap();
            git_in(dir, &["add", "."]);
            git_in(dir, &["commit", "-m", f]);
        }
        assert_eq!(count_commits_since_last_tag_in(dir, None).unwrap(), 2);
        // A newer version tag lands -> counter resets to 0 at the tag.
        git_in(dir, &["tag", "v1.1.0"]);
        assert_eq!(count_commits_since_last_tag_in(dir, None).unwrap(), 0);
        std::fs::write(dir.join("d"), "x").unwrap();
        git_in(dir, &["add", "."]);
        git_in(dir, &["commit", "-m", "d"]);
        assert_eq!(count_commits_since_last_tag_in(dir, None).unwrap(), 1);
    }

    #[test]
    fn count_commits_since_last_tag_counts_all_when_no_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commits(dir, &["a", "b", "c"]);
        // No tag at all -> count every commit on HEAD.
        assert_eq!(count_commits_since_last_tag_in(dir, None).unwrap(), 3);
    }

    #[test]
    fn count_commits_since_last_tag_respects_monorepo_prefix() {
        // Per-crate workspace: tags for two subprojects interleave on one
        // branch. The `core/` count must be since the latest `core/*` tag,
        // NOT the nearer `api/*` tag from a different subproject.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commits(dir, &["a"]);
        git_in(dir, &["tag", "core/v1.0.0"]); // matching-prefix tag (older)
        for f in ["b", "c"] {
            std::fs::write(dir.join(f), "x").unwrap();
            git_in(dir, &["add", "."]);
            git_in(dir, &["commit", "-m", f]);
        }
        git_in(dir, &["tag", "api/v2.0.0"]); // DIFFERENT prefix, NEARER to HEAD
        std::fs::write(dir.join("d"), "x").unwrap();
        git_in(dir, &["add", "."]);
        git_in(dir, &["commit", "-m", "d"]);

        // With prefix filtering: count since core/v1.0.0 = 3 commits (b, c, d).
        assert_eq!(
            count_commits_since_last_tag_in(dir, Some("core/")).unwrap(),
            3,
            "must count since the matching-prefix tag, ignoring api/v2.0.0",
        );
        // Without filtering (None): describe picks the nearer api/v2.0.0,
        // so the count is only 1 (d). This is the mutation-check baseline
        // proving the --match arg is load-bearing.
        assert_eq!(
            count_commits_since_last_tag_in(dir, None).unwrap(),
            1,
            "unfiltered count picks the nearest (wrong) subproject tag",
        );
    }

    #[test]
    fn get_current_branch_in_returns_branch_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run = |args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            );
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["-c", "init.defaultBranch=t1-test-branch", "init"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a"), "1").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "c1"]);
        let branch = get_current_branch_in(dir).unwrap();
        assert_eq!(branch, "t1-test-branch");
    }

    #[test]
    fn get_current_branch_in_resolves_detached_head_via_points_at() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run = |args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            );
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["-c", "init.defaultBranch=master", "init"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a"), "1").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "c1"]);
        let sha = get_head_commit_in(dir).unwrap();
        run(&["checkout", "--detach", &sha]);
        let branch = get_current_branch_in(dir).unwrap();
        assert_eq!(
            branch, "master",
            "detached HEAD pointing at master must resolve to master, not literal HEAD"
        );
    }

    #[test]
    fn is_branchlike_rejects_lockstep_tag_shapes() {
        assert!(!is_branchlike("v0.4.5"));
        assert!(!is_branchlike("v1.2.3"));
        assert!(!is_branchlike("v10.20.30"));
        assert!(!is_branchlike("v1.2.3-rc.1"));
        assert!(!is_branchlike("v1.2.3+build.42"));
    }

    #[test]
    fn is_branchlike_rejects_per_crate_tag_shapes() {
        assert!(!is_branchlike("mycrate-v1.2.3"));
        assert!(!is_branchlike("cfgd-operator-v0.4.0"));
        assert!(!is_branchlike("anodize-core-v1.2.3-rc.1"));
    }

    #[test]
    fn is_branchlike_accepts_real_branch_names() {
        assert!(is_branchlike("master"));
        assert!(is_branchlike("main"));
        assert!(is_branchlike("publisher-required-config"));
        assert!(is_branchlike("release/v1.2.3-prep"));
        assert!(is_branchlike("dependabot/cargo/serde-1.0.200"));
    }

    #[test]
    fn is_branchlike_accepts_slashed_branch_with_embedded_version() {
        // `feature/fix-v2.0.0` embeds `-v2.0.0` but is a branch, not a
        // per-crate tag: the unanchored `-v\d+\.\d+\.\d+` regex misclassified
        // it as a tag. The `^[^/]+-v` anchor keeps slashed branch names
        // branch-like.
        assert!(is_branchlike("feature/fix-v2.0.0"));
        assert!(is_branchlike("hotfix/release-v1.0.0-blocker"));
        assert!(is_branchlike("user/wip-v3.1.4"));
    }

    #[test]
    fn get_current_branch_in_rejects_tag_shaped_github_ref_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run = |args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            );
            assert!(out.status.success(), "git {args:?} failed");
        };
        // Build a repo whose HEAD is detached AND no local branch points
        // at it, so every fallback BEFORE GITHUB_REF_NAME fails. The only
        // way the fallback chain produces a value is via the env var.
        run(&["-c", "init.defaultBranch=master", "init"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a"), "1").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "c1"]);
        let sha = get_head_commit_in(dir).unwrap();
        // Move master forward so the detached HEAD has no branch
        // pointing at it; for-each-ref --points-at HEAD returns empty.
        std::fs::write(dir.join("a"), "2").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "c2"]);
        run(&["checkout", "--detach", &sha]);

        // GITHUB_REF_NAME is injected via the env seam, so each branch of the
        // fallback is driven without mutating process-global env.

        // Tag-shaped: must NOT be accepted; bail surfaces.
        let env = crate::MapEnvSource::new().with("GITHUB_REF_NAME", "v0.4.5");
        let err = get_current_branch_in_with_env(dir, &env).unwrap_err();
        assert!(
            err.to_string().contains("could not resolve current branch"),
            "tag-shaped GITHUB_REF_NAME must trigger bail: {err}"
        );

        // Per-crate-shaped: must NOT be accepted either.
        let env = crate::MapEnvSource::new().with("GITHUB_REF_NAME", "mycrate-v1.2.3");
        let err = get_current_branch_in_with_env(dir, &env).unwrap_err();
        assert!(
            err.to_string().contains("could not resolve current branch"),
            "per-crate tag GITHUB_REF_NAME must trigger bail: {err}"
        );

        // Real branch name: accepted.
        let env = crate::MapEnvSource::new().with("GITHUB_REF_NAME", "master");
        let branch = get_current_branch_in_with_env(dir, &env).unwrap();
        assert_eq!(branch, "master");
    }

    #[test]
    fn branches_containing_sha_in_returns_empty_without_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run = |args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            );
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["-c", "init.defaultBranch=master", "init"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a"), "1").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "c1"]);
        let sha = get_head_commit_in(dir).unwrap();
        // No remote configured → `git branch -r --contains` returns
        // empty, which the helper surfaces as `Vec::new()` so the
        // caller can fall back to local branch resolution.
        let branches = branches_containing_sha_in(dir, &sha).unwrap();
        assert!(branches.is_empty(), "no remote → no remote branches");
    }

    #[test]
    fn branches_containing_sha_in_finds_remote_branch_after_push() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run_in = |cwd: &Path, args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(cwd)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            );
            assert!(out.status.success(), "git {args:?} failed");
        };
        run_in(
            bare.path(),
            &["-c", "init.defaultBranch=master", "init", "--bare"],
        );
        run_in(dir, &["-c", "init.defaultBranch=master", "init"]);
        run_in(dir, &["config", "user.email", "t@t.com"]);
        run_in(dir, &["config", "user.name", "t"]);
        run_in(
            dir,
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        std::fs::write(dir.join("a"), "1").unwrap();
        run_in(dir, &["add", "."]);
        run_in(dir, &["commit", "-m", "c1"]);
        let sha = get_head_commit_in(dir).unwrap();
        run_in(dir, &["push", "-u", "origin", "master"]);

        let branches = branches_containing_sha_in(dir, &sha).unwrap();
        assert_eq!(branches, vec!["master".to_string()]);
    }

    #[test]
    fn stage_and_commit_in_returns_false_when_no_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commits(dir, &["a"]);
        // File is committed and unchanged — staging it should not produce
        // a diff, and stage_and_commit must report Ok(false) instead of
        // bailing on the "nothing to commit" path.
        let created = stage_and_commit_in(dir, &["a"], "chore: should be a no-op").unwrap();
        assert!(!created, "no diff → no commit should be created");
        let log = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["log", "--oneline"]).current_dir(dir);
                cmd
            },
            "git",
        );
        let log_text = String::from_utf8_lossy(&log.stdout);
        assert!(
            !log_text.contains("should be a no-op"),
            "stage_and_commit_in must not create a commit when no diff: {log_text}"
        );
    }

    #[test]
    fn stage_and_commit_in_returns_true_when_file_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commits(dir, &["a"]);
        std::fs::write(dir.join("a"), "changed").unwrap();
        let created = stage_and_commit_in(dir, &["a"], "chore: real change").unwrap();
        assert!(created, "real change → commit must be created");
        let log = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["log", "-1", "--pretty=%s"]).current_dir(dir);
                cmd
            },
            "git",
        );
        let subject = String::from_utf8_lossy(&log.stdout).trim().to_string();
        assert_eq!(subject, "chore: real change");
    }

    #[test]
    fn git_output_in_error_falls_back_to_stdout_when_stderr_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo_with_commits(dir, &["a"]);
        // `git commit -m ...` with an unchanged tree prints "nothing to
        // commit" to STDOUT (not stderr); the error message must surface
        // that detail instead of `failed: ` with nothing after.
        let err = git_output_in(dir, &["commit", "-m", "no-op"]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("nothing to commit") || msg.contains("clean"),
            "error must include stdout detail when stderr is empty: {msg}"
        );
    }

    /// `CommitterIdentity::default_for_rollback` produces a populated
    /// (name + email) identity. The exact host-derived suffix isn't
    /// load-bearing — what matters is that both fields are present so
    /// `apply_to` produces all four `GIT_AUTHOR_*` / `GIT_COMMITTER_*`
    /// envs on the spawn.
    #[test]
    fn default_for_rollback_populates_both_name_and_email() {
        let id = CommitterIdentity::default_for_rollback();
        assert_eq!(id.name.as_deref(), Some("anodize-rollback"));
        let email = id.email.expect("email must be Some");
        assert!(
            email.starts_with("anodize-rollback@"),
            "email must use the anodize-rollback@<host> shape; got {email}"
        );
        assert!(!email.ends_with('@'), "host portion must not be empty");
    }

    /// `revert_commit_in` with an injected `CommitterIdentity` writes a
    /// commit whose author/committer match the identity. Exercises the
    /// env-injection path end-to-end against a real fixture repo whose
    /// only configured identity is the override — so a future regression
    /// that drops the env threading would show up as the commit
    /// inheriting the host's `user.email` instead.
    #[test]
    fn revert_commit_in_uses_injected_identity_envs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run_env = |args: &[&str], extra: &[(&str, &str)]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir)
                        .env("GIT_AUTHOR_NAME", "bootstrap")
                        .env("GIT_AUTHOR_EMAIL", "bootstrap@b.com")
                        .env("GIT_COMMITTER_NAME", "bootstrap")
                        .env("GIT_COMMITTER_EMAIL", "bootstrap@b.com");
                    for (k, v) in extra {
                        cmd.env(k, v);
                    }
                    cmd
                },
                "git",
            );
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_env(&["init", "-b", "master"], &[]);
        std::fs::write(dir.join("a"), "0").unwrap();
        run_env(&["add", "."], &[]);
        run_env(&["commit", "-m", "initial"], &[]);
        std::fs::write(dir.join("a"), "1").unwrap();
        run_env(&["add", "."], &[]);
        run_env(&["commit", "-m", "chore(release): v1.0.0"], &[]);
        let bump_sha = get_head_commit_in(dir).unwrap();

        // Inject a distinct identity so the resulting revert commit can
        // be attributed unambiguously to the env path (the bootstrap
        // commits used a different identity above).
        let identity = CommitterIdentity {
            name: Some("rollback-bot".to_string()),
            email: Some("rollback-bot@anodize.test".to_string()),
        };
        revert_commit_in(dir, &bump_sha, Some("chore(release): rollback"), &identity)
            .expect("revert with injected identity must succeed");

        // The new HEAD commit's author email must be the injected one,
        // proving the env threading reached the git child.
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.current_dir(dir)
                    .args(["log", "-1", "--format=%ae"])
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .env("LC_ALL", "C");
                cmd
            },
            "git",
        );
        let author_email = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(
            author_email, "rollback-bot@anodize.test",
            "revert commit must carry the injected committer identity"
        );

        // Repo config must remain unchanged — env-only fallback, no
        // `git config user.email ...` mutation.
        let cfg = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.current_dir(dir)
                    .args(["config", "--local", "--get", "user.email"])
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .env("LC_ALL", "C");
                cmd
            },
            "git",
        );
        assert!(
            !cfg.status.success() || cfg.stdout.is_empty(),
            "revert must not write user.email into the repo's local config; got: {}",
            String::from_utf8_lossy(&cfg.stdout)
        );
    }

    /// B-R4: a revert that hits conflicts (because later commits overlap
    /// with the bump) must run `git revert --abort`, restoring the working
    /// tree so the operator isn't trapped by the dirty-tree guard on the
    /// next attempt. Bail message must mention "aborted".
    #[test]
    fn revert_commit_in_aborts_on_conflict_and_leaves_tree_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run = |args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            );
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-b", "master"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "t"]);
        // Initial commit: file `x` with line "v1".
        std::fs::write(dir.join("x"), "v1\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
        // "Bump" commit: change to "v2".
        std::fs::write(dir.join("x"), "v2\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "chore(release): v2"]);
        let bump_sha = get_head_commit_in(dir).unwrap();
        // Later overlapping commit: change to "v3". A revert of the bump
        // would try to restore "v1" from a base of "v2", but HEAD is now
        // "v3" — that's the canonical revert-conflict shape.
        std::fs::write(dir.join("x"), "v3\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "feat: overlap"]);

        let identity = CommitterIdentity::default();
        let err = revert_commit_in(dir, &bump_sha, None, &identity)
            .expect_err("revert against overlapping HEAD must conflict and bail");
        let msg = format!("{err}");
        assert!(
            msg.contains("aborted"),
            "bail message must mention abort recovery: {msg}"
        );

        // Working tree must be clean post-bail: no REVERT_HEAD, no
        // unmerged paths. The next rollback attempt must NOT hit the
        // dirty-tree guard.
        assert!(
            !dir.join(".git/REVERT_HEAD").exists(),
            ".git/REVERT_HEAD must be cleaned up after --abort"
        );
        let status_out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["status", "--porcelain"]).current_dir(dir);
                cmd
            },
            "git",
        );
        assert!(
            status_out.stdout.is_empty(),
            "working tree must be clean after revert --abort; got:\n{}",
            String::from_utf8_lossy(&status_out.stdout)
        );
    }

    /// S-R7: `commits_with_subjects_in` returns every (sha, subject)
    /// pair in one git spawn. Asserts both correctness (matches per-commit
    /// `commit_subject_in`) and that the range bound is exclusive on the
    /// `<sha>` side.
    #[test]
    fn commits_with_subjects_in_returns_all_pairs_in_one_call() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run = |args: &[&str]| {
            let out = anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(args)
                        .current_dir(dir)
                        .env("GIT_AUTHOR_NAME", "t")
                        .env("GIT_AUTHOR_EMAIL", "t@t.com")
                        .env("GIT_COMMITTER_NAME", "t")
                        .env("GIT_COMMITTER_EMAIL", "t@t.com");
                    cmd
                },
                "git",
            );
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["init", "-b", "master"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a"), "0").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
        let base = get_head_commit_in(dir).unwrap();
        std::fs::write(dir.join("a"), "1").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "feat: A with extra detail"]);
        std::fs::write(dir.join("a"), "2").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "fix: B"]);

        let pairs = commits_with_subjects_in(dir, &base).unwrap();
        assert_eq!(pairs.len(), 2, "two commits sit on top of base");
        // Newest-first ordering (matches `git log` default).
        assert_eq!(pairs[0].1, "fix: B");
        assert_eq!(pairs[1].1, "feat: A with extra detail");

        // Empty range (sha IS HEAD) → empty vec.
        let head = get_head_commit_in(dir).unwrap();
        assert!(commits_with_subjects_in(dir, &head).unwrap().is_empty());
    }

    #[test]
    fn parse_commit_output_with_files_pairs_each_commit_with_its_files() {
        // Two commits: newest first (git log order). Each metadata record is
        // `%H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e`, then `--name-only` files.
        let raw = "h1\x1fs1\x1ffix: B\x1ft\x1ft@t\x1f\x1e\ncrates/cli/main.rs\n\nh0\x1fs0\x1ffeat: A\x1ft\x1ft@t\x1f\x1e\ncrates/core/lib.rs\nCargo.toml\n";
        let parsed = parse_commit_output_with_files(raw);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].commit.message, "fix: B");
        assert_eq!(parsed[0].files, vec!["crates/cli/main.rs".to_string()]);
        assert_eq!(parsed[1].commit.message, "feat: A");
        assert_eq!(
            parsed[1].files,
            vec!["crates/core/lib.rs".to_string(), "Cargo.toml".to_string()]
        );
    }

    #[test]
    fn parse_commit_output_with_files_preserves_multiline_body_at_idx_gt_0() {
        // A multi-line `%b` body for the SECOND commit (idx>0): the body spans
        // several newline-separated lines, and the parser must keep the full
        // record — not just its first line — so trailers like `Co-Authored-By:`
        // survive, matching the metadata-only `parse_git_log_records` path.
        let body0 = "detail line one\ndetail line two\n\nCo-Authored-By: Bob <bob@b.com>";
        let raw = format!(
            "h1\x1fs1\x1ffix: B\x1ft\x1ft@t\x1f\x1e\ncrates/cli/main.rs\n\n\
             h0\x1fs0\x1ffeat: A\x1ft\x1ft@t\x1f{body0}\x1e\ncrates/core/lib.rs\n"
        );
        let parsed = parse_commit_output_with_files(&raw);
        assert_eq!(parsed.len(), 2);
        // The idx>0 commit retains its FULL multi-line body and trailer.
        assert_eq!(parsed[1].commit.message, "feat: A");
        assert_eq!(parsed[1].commit.body, body0);
        assert!(
            parsed[1]
                .commit
                .body
                .contains("Co-Authored-By: Bob <bob@b.com>"),
            "multi-line body trailer dropped: {:?}",
            parsed[1].commit.body
        );
        assert_eq!(parsed[1].files, vec!["crates/core/lib.rs".to_string()]);
    }

    #[test]
    fn get_commits_between_paths_with_files_in_reports_touched_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run = |args: &[&str]| {
            assert!(
                anodizer_core::test_helpers::output_with_spawn_retry(
                    || {
                        let mut cmd = Command::new("git");
                        cmd.args(args)
                            .current_dir(dir)
                            .env("GIT_AUTHOR_NAME", "t")
                            .env("GIT_AUTHOR_EMAIL", "t@t.com")
                            .env("GIT_COMMITTER_NAME", "t")
                            .env("GIT_COMMITTER_EMAIL", "t@t.com");
                        cmd
                    },
                    "git",
                )
                .status
                .success()
            );
        };
        run(&["init"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("base"), "0").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
        let base = get_head_commit_in(dir).unwrap();
        std::fs::create_dir_all(dir.join("crates/core")).unwrap();
        std::fs::write(dir.join("crates/core/lib.rs"), "1").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "feat: core"]);

        let pairs = get_commits_between_paths_with_files_in(dir, &base, "HEAD", &[]).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].commit.message, "feat: core");
        assert_eq!(pairs[0].files, vec!["crates/core/lib.rs".to_string()]);
    }

    #[test]
    fn get_commits_between_paths_with_files_in_preserves_multiline_body_for_later_commits() {
        // Real `git log --name-only` over TWO post-base commits, the OLDER one
        // (idx>0 in the newest-first output) carrying a multi-line body with a
        // `Co-Authored-By:` trailer. The full body must survive — proving the
        // narrowed fetch path agrees with the metadata-only path on body
        // content, not just the subject.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run = |args: &[&str]| {
            assert!(
                anodizer_core::test_helpers::output_with_spawn_retry(
                    || {
                        let mut cmd = Command::new("git");
                        cmd.args(args)
                            .current_dir(dir)
                            .env("GIT_AUTHOR_NAME", "t")
                            .env("GIT_AUTHOR_EMAIL", "t@t.com")
                            .env("GIT_COMMITTER_NAME", "t")
                            .env("GIT_COMMITTER_EMAIL", "t@t.com");
                        cmd
                    },
                    "git",
                )
                .status
                .success()
            );
        };
        run(&["init"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("base"), "0").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
        let base = get_head_commit_in(dir).unwrap();

        // Older of the two reported commits — multi-line body + trailer.
        std::fs::write(dir.join("a.rs"), "1").unwrap();
        run(&["add", "."]);
        run(&[
            "commit",
            "-m",
            "feat: with body\n\nfirst body line\nsecond body line\n\nCo-Authored-By: Bob <bob@b.com>",
        ]);
        // Newer commit (idx 0 in newest-first output), single-line.
        std::fs::write(dir.join("b.rs"), "2").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "fix: later"]);

        let pairs = get_commits_between_paths_with_files_in(dir, &base, "HEAD", &[]).unwrap();
        assert_eq!(pairs.len(), 2);
        // Newest-first: [0] = "fix: later", [1] = "feat: with body" (idx>0).
        assert_eq!(pairs[0].commit.message, "fix: later");
        let body = &pairs[1].commit.body;
        assert!(
            body.contains("first body line") && body.contains("second body line"),
            "multi-line body truncated for idx>0 commit: {body:?}"
        );
        assert!(
            body.contains("Co-Authored-By: Bob <bob@b.com>"),
            "Co-Authored-By trailer dropped for idx>0 commit: {body:?}"
        );
    }

    // ---- parse_commit_output: the single wire-format record decoder ----

    #[test]
    fn parse_commit_output_empty_input_yields_no_commits() {
        assert!(parse_commit_output("").is_empty());
    }

    #[test]
    fn parse_commit_output_decodes_all_six_fields() {
        // %H%x1f%h%x1f%s%x1f%an%x1f%ae%x1f%b%x1e for a single commit.
        let raw =
            "abc123def\x1fabc123d\x1ffeat: add thing\x1fAlice\x1falice@x.com\x1fbody text\x1e";
        let commits = parse_commit_output(raw);
        assert_eq!(commits.len(), 1);
        let c = &commits[0];
        assert_eq!(c.hash, "abc123def");
        assert_eq!(c.short_hash, "abc123d");
        assert_eq!(c.message, "feat: add thing");
        assert_eq!(c.author_name, "Alice");
        assert_eq!(c.author_email, "alice@x.com");
        assert_eq!(c.body, "body text");
    }

    #[test]
    fn parse_commit_output_trims_hash_and_body_but_keeps_inner_subject() {
        // Per the decoder: hash and body are trimmed; the subject (field 2)
        // is taken verbatim. A leading-newline body must come back trimmed.
        let raw = "  abc  \x1fabc\x1ffix: keep  spaces\x1ft\x1ft@t\x1f\n\nbody\n\x1e";
        let commits = parse_commit_output(raw);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].hash, "abc", "hash is trimmed");
        assert_eq!(commits[0].message, "fix: keep  spaces", "subject verbatim");
        assert_eq!(commits[0].body, "body", "body is trimmed");
    }

    #[test]
    fn parse_commit_output_absent_body_field_defaults_to_empty() {
        // Exactly 5 fields (no %b segment) is still a valid record; body == "".
        let raw = "h\x1fh\x1fsubject\x1fname\x1fmail\x1e";
        let commits = parse_commit_output(raw);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].body, "");
        assert_eq!(commits[0].message, "subject");
    }

    #[test]
    fn parse_commit_output_skips_records_with_too_few_fields() {
        // A record with <5 unit-separated fields is malformed and dropped,
        // while a well-formed sibling record in the same stream survives.
        let raw = "only\x1ftwo\x1e\
                   h\x1fh\x1fgood: subject\x1fn\x1fe\x1fbody\x1e";
        let commits = parse_commit_output(raw);
        assert_eq!(commits.len(), 1, "malformed record dropped, good one kept");
        assert_eq!(commits[0].message, "good: subject");
    }

    #[test]
    fn parse_commit_output_multiline_body_survives_record_separator_split() {
        // Two commits separated by \x1e; the first body spans newlines and
        // carries a trailer — the \x1e (not \n) split keeps it intact.
        let raw = "h1\x1fh1\x1ffeat: A\x1fA\x1fa@x\x1fline one\nline two\n\nCo-Authored-By: B <b@x>\x1e\
                   h0\x1fh0\x1ffix: B\x1fB\x1fb@x\x1f\x1e";
        let commits = parse_commit_output(raw);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].message, "feat: A");
        assert!(commits[0].body.contains("line one\nline two"));
        assert!(commits[0].body.contains("Co-Authored-By: B <b@x>"));
        assert_eq!(commits[1].message, "fix: B");
        assert_eq!(commits[1].body, "");
    }

    // ---- short_commit_str: pure SHA truncation ----

    #[test]
    fn short_commit_str_truncates_long_sha_to_seven() {
        assert_eq!(short_commit_str("abcdef0123456789"), "abcdef0");
        assert_eq!(short_commit_str("abcdef0123456789").len(), SHORT_COMMIT_LEN);
    }

    #[test]
    fn short_commit_str_returns_shorter_or_equal_input_unchanged() {
        assert_eq!(short_commit_str("abc"), "abc", "shorter than 7 unchanged");
        assert_eq!(
            short_commit_str("abcdefg"),
            "abcdefg",
            "exactly 7 unchanged"
        );
        assert_eq!(short_commit_str(""), "", "empty stays empty");
    }

    // ---- real-repo fixture helpers for the shelling functions ----

    /// Run a git command in `dir` with a pinned identity, asserting success.
    fn g(dir: &Path, args: &[&str]) {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "Ada")
                    .env("GIT_AUTHOR_EMAIL", "ada@x.com")
                    .env("GIT_COMMITTER_NAME", "Ada")
                    .env("GIT_COMMITTER_EMAIL", "ada@x.com")
                    .env("GIT_AUTHOR_DATE", "1715000000 +0000")
                    .env("GIT_COMMITTER_DATE", "1715000000 +0000");
                cmd
            },
            "git",
        );
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// `git init -b master` + identity config; no commits yet.
    fn init_bare_repo(dir: &Path) {
        g(dir, &["init", "-b", "master"]);
        g(dir, &["config", "user.email", "ada@x.com"]);
        g(dir, &["config", "user.name", "Ada"]);
    }

    /// Write `path`=`content`, stage all, commit with `subject`.
    fn commit_file(dir: &Path, path: &str, content: &str, subject: &str) {
        let full = dir.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
        g(dir, &["add", "."]);
        g(dir, &["commit", "-m", subject]);
    }

    // ---- get_commits_between_in / paths variants ----

    #[test]
    fn get_commits_between_in_returns_only_post_base_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "initial");
        let base = get_head_commit_in(dir).unwrap();
        commit_file(dir, "a", "1", "feat: one");
        commit_file(dir, "a", "2", "fix: two");

        let commits = get_commits_between_in(dir, &base, "HEAD", None).unwrap();
        assert_eq!(commits.len(), 2, "two commits sit above base");
        // git log default is newest-first.
        assert_eq!(commits[0].message, "fix: two");
        assert_eq!(commits[1].message, "feat: one");
        assert_eq!(commits[1].author_name, "Ada");
        assert_eq!(commits[1].author_email, "ada@x.com");
    }

    #[test]
    fn get_commits_between_in_path_filter_excludes_untouched_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "base", "0", "initial");
        let base = get_head_commit_in(dir).unwrap();
        commit_file(dir, "src/lib.rs", "1", "feat: touch lib");
        commit_file(dir, "docs/readme", "2", "docs: touch docs only");

        // Filter to src/ — only the lib commit should be reported.
        let commits = get_commits_between_in(dir, &base, "HEAD", Some("src")).unwrap();
        assert_eq!(commits.len(), 1, "only the src-touching commit survives");
        assert_eq!(commits[0].message, "feat: touch lib");
    }

    #[test]
    fn get_commits_between_paths_in_unions_multiple_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "base", "0", "initial");
        let base = get_head_commit_in(dir).unwrap();
        commit_file(dir, "a/x", "1", "feat: a");
        commit_file(dir, "b/y", "2", "feat: b");
        commit_file(dir, "c/z", "3", "feat: c");

        // Two paths -> union of commits touching either a/ or b/.
        let commits =
            get_commits_between_paths_in(dir, &base, "HEAD", &["a".into(), "b".into()]).unwrap();
        let subjects: Vec<&str> = commits.iter().map(|c| c.message.as_str()).collect();
        assert_eq!(
            commits.len(),
            2,
            "a and b touched, c excluded: {subjects:?}"
        );
        assert!(subjects.contains(&"feat: a"));
        assert!(subjects.contains(&"feat: b"));
        assert!(!subjects.contains(&"feat: c"));
    }

    // ---- get_all_commits_* ----

    #[test]
    fn get_all_commits_in_returns_every_commit_on_head() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "first");
        commit_file(dir, "a", "1", "second");
        commit_file(dir, "a", "2", "third");

        let commits = get_all_commits_in(dir, None).unwrap();
        assert_eq!(commits.len(), 3);
        assert_eq!(commits[0].message, "third", "newest-first");
        assert_eq!(commits[2].message, "first");
    }

    #[test]
    fn get_all_commits_paths_in_filters_to_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "keep/x", "0", "feat: keep");
        commit_file(dir, "drop/y", "1", "feat: drop");

        let commits = get_all_commits_paths_in(dir, &["keep".into()]).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].message, "feat: keep");
    }

    #[test]
    fn get_all_commits_paths_with_files_in_pairs_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "crates/core/lib.rs", "0", "feat: core");

        let pairs = get_all_commits_paths_with_files_in(dir, &[]).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].commit.message, "feat: core");
        assert_eq!(pairs[0].files, vec!["crates/core/lib.rs".to_string()]);
    }

    // ---- get_commits_reachable_paths_in: bound at an explicit ref ----

    #[test]
    fn get_commits_reachable_paths_in_stops_at_the_given_rev() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "first");
        commit_file(dir, "a", "1", "second");
        let mid = get_head_commit_in(dir).unwrap();
        commit_file(dir, "a", "2", "third-after-mid");

        // Reachable from `mid` excludes the commit made after it.
        let commits = get_commits_reachable_paths_in(dir, &mid, &[]).unwrap();
        let subjects: Vec<&str> = commits.iter().map(|c| c.message.as_str()).collect();
        assert_eq!(commits.len(), 2, "only ancestors of mid: {subjects:?}");
        assert!(subjects.contains(&"first"));
        assert!(subjects.contains(&"second"));
        assert!(!subjects.contains(&"third-after-mid"));
    }

    #[test]
    fn get_commits_reachable_paths_with_files_in_pairs_touched_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "src/main.rs", "0", "feat: main");
        let head = get_head_commit_in(dir).unwrap();

        let pairs = get_commits_reachable_paths_with_files_in(dir, &head, &[]).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].commit.message, "feat: main");
        assert_eq!(pairs[0].files, vec!["src/main.rs".to_string()]);
    }

    // ---- subject-only message helpers ----

    #[test]
    fn get_last_commit_messages_in_returns_n_subjects_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "one");
        commit_file(dir, "a", "1", "two");
        commit_file(dir, "a", "2", "three");

        let msgs = get_last_commit_messages_in(dir, 2).unwrap();
        assert_eq!(msgs, vec!["three".to_string(), "two".to_string()]);
    }

    #[test]
    fn get_commit_messages_between_in_lists_post_base_subjects() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "initial");
        let base = get_head_commit_in(dir).unwrap();
        commit_file(dir, "a", "1", "feat: x");
        commit_file(dir, "a", "2", "fix: y");

        let msgs = get_commit_messages_between_in(dir, &base, "HEAD").unwrap();
        assert_eq!(msgs, vec!["fix: y".to_string(), "feat: x".to_string()]);
    }

    #[test]
    fn get_last_commit_messages_path_in_filters_to_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "keep/a", "0", "feat: keep");
        commit_file(dir, "other/b", "1", "feat: other");

        let msgs = get_last_commit_messages_path_in(dir, 10, "keep").unwrap();
        assert_eq!(msgs, vec!["feat: keep".to_string()]);
    }

    #[test]
    fn get_commit_messages_between_path_in_filters_range_and_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "base", "0", "initial");
        let base = get_head_commit_in(dir).unwrap();
        commit_file(dir, "src/x", "1", "feat: src");
        commit_file(dir, "doc/y", "2", "docs: doc");

        let msgs = get_commit_messages_between_path_in(dir, &base, "HEAD", "src").unwrap();
        assert_eq!(msgs, vec!["feat: src".to_string()]);
    }

    // ---- diff / change-detection helpers ----

    #[test]
    fn has_changes_since_in_detects_path_touched_after_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "watched", "0", "initial");
        g(dir, &["tag", "v1.0.0"]);
        // No change yet -> false.
        assert!(!has_changes_since_in(dir, "v1.0.0", "watched").unwrap());
        commit_file(dir, "watched", "1", "feat: change watched");
        // Now changed -> true.
        assert!(has_changes_since_in(dir, "v1.0.0", "watched").unwrap());
        // A different, untouched path -> false.
        assert!(!has_changes_since_in(dir, "v1.0.0", "unrelated").unwrap());
    }

    #[test]
    fn paths_changed_since_tag_in_true_when_any_path_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "initial");
        g(dir, &["tag", "v1.0.0"]);
        commit_file(dir, "b", "1", "feat: add b");

        // b changed; checking [a, b] -> true (b matched).
        assert!(paths_changed_since_tag_in(dir, "v1.0.0", &["a", "b"]).unwrap());
        // Only a (unchanged) -> false.
        assert!(!paths_changed_since_tag_in(dir, "v1.0.0", &["a"]).unwrap());
    }

    #[test]
    fn paths_changed_since_tag_in_returns_false_when_git_fails() {
        // Non-existent tag makes `git diff` fail; the helper maps that to
        // Ok(false) rather than bubbling an error.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "initial");
        assert!(!paths_changed_since_tag_in(dir, "nope-no-such-tag", &["a"]).unwrap());
    }

    // ---- rev resolution helpers ----

    #[test]
    fn head_commit_hash_in_matches_rev_parse_head() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "initial");
        let expected = get_head_commit_in(dir).unwrap();
        assert_eq!(head_commit_hash_in(dir).unwrap(), expected);
    }

    #[test]
    fn head_commit_hash_in_errors_on_non_repo() {
        let tmp = tempfile::tempdir().unwrap();
        // No git init -> rev-parse HEAD fails.
        assert!(head_commit_hash_in(tmp.path()).is_err());
    }

    #[test]
    fn rev_parse_in_resolves_branch_to_full_sha() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "initial");
        let head = get_head_commit_in(dir).unwrap();
        assert_eq!(rev_parse_in(dir, "master").unwrap(), head);
    }

    #[test]
    fn rev_verify_commit_in_accepts_commit_rejects_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "initial");
        let head = get_head_commit_in(dir).unwrap();
        assert_eq!(rev_verify_commit_in(dir, "HEAD").unwrap(), head);
        // A made-up ref must not verify.
        assert!(rev_verify_commit_in(dir, "deadbeefdeadbeef").is_err());
    }

    #[test]
    fn commits_between_in_lists_shas_above_base_and_empty_at_head() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "initial");
        let base = get_head_commit_in(dir).unwrap();
        commit_file(dir, "a", "1", "second");
        let head = get_head_commit_in(dir).unwrap();

        let shas = commits_between_in(dir, &base).unwrap();
        assert_eq!(
            shas,
            vec![head.clone()],
            "exactly the one commit above base"
        );
        // sha IS HEAD -> empty range.
        assert!(commits_between_in(dir, &head).unwrap().is_empty());
    }

    #[test]
    fn commit_subject_in_returns_single_commit_subject() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "feat: only-subject\n\nignored body");
        let head = get_head_commit_in(dir).unwrap();
        assert_eq!(commit_subject_in(dir, &head).unwrap(), "feat: only-subject");
    }

    #[test]
    fn head_commit_timestamp_in_returns_pinned_committer_epoch() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        // Pinned GIT_COMMITTER_DATE in `g` is 1715000000 +0000.
        commit_file(dir, "a", "0", "initial");
        assert_eq!(head_commit_timestamp_in(dir).unwrap(), 1_715_000_000);
    }

    // ---- log_subjects_for_range ----

    #[test]
    fn log_subjects_for_range_returns_full_bodies_for_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "watched", "0", "feat: A\n\nbody of A");
        commit_file(dir, "watched", "1", "fix: B");

        let bodies = log_subjects_for_range(dir, "HEAD", "watched").unwrap();
        assert_eq!(bodies.len(), 2);
        // %B is subject+body; newest-first.
        assert!(bodies[0].starts_with("fix: B"));
        assert!(bodies[1].contains("feat: A") && bodies[1].contains("body of A"));
    }

    #[test]
    fn log_subjects_for_range_returns_empty_when_range_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "initial");
        // A range referencing a non-existent ref makes git fail; the helper
        // maps that to an empty Vec, not an error.
        let bodies = log_subjects_for_range(dir, "no-such-ref..HEAD", "a").unwrap();
        assert!(bodies.is_empty());
    }

    /// A pathspec fatal (empty pathspec) is a REAL failure, not an empty
    /// history — collapsing it into `Ok(vec![])` made `bump` preview Skip
    /// for root-level crates.
    #[test]
    fn log_subjects_for_range_propagates_pathspec_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "initial");
        let err = log_subjects_for_range(dir, "HEAD", "")
            .expect_err("empty pathspec must be an error, not empty success")
            .to_string();
        assert!(err.contains("git log HEAD failed"), "{err}");
    }

    // ---- add_path_in + commit_in ----

    #[test]
    fn add_path_in_then_commit_in_creates_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "seed", "0", "initial");
        std::fs::write(dir.join("new.txt"), "hello").unwrap();

        add_path_in(dir, std::path::Path::new("new.txt")).unwrap();
        commit_in(dir, "feat: add new.txt", false).unwrap();

        let subject = String::from_utf8(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["log", "-1", "--pretty=%s"]).current_dir(dir);
                    cmd
                },
                "git",
            )
            .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        assert_eq!(subject, "feat: add new.txt");
    }

    #[test]
    fn add_path_in_errors_on_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        let err = add_path_in(dir, std::path::Path::new("does-not-exist")).unwrap_err();
        assert!(
            err.to_string().contains("git add"),
            "error must name the failing git add: {err}"
        );
    }

    // ---- reset_hard_in ----

    #[test]
    fn reset_hard_in_moves_head_and_restores_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "first", "first");
        let target = get_head_commit_in(dir).unwrap();
        commit_file(dir, "a", "second", "second");
        assert_ne!(get_head_commit_in(dir).unwrap(), target);

        reset_hard_in(dir, &target).unwrap();
        assert_eq!(get_head_commit_in(dir).unwrap(), target, "HEAD moved back");
        assert_eq!(
            std::fs::read_to_string(dir.join("a")).unwrap(),
            "first",
            "working tree restored to target content"
        );
    }

    // ---- push_branch_in error path (no remote) ----

    #[test]
    fn push_branch_in_bails_without_origin_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir);
        commit_file(dir, "a", "0", "initial");
        let err = push_branch_in(dir, "master").unwrap_err();
        assert!(
            err.to_string().contains("no 'origin' remote"),
            "missing-remote bail must be explicit: {err}"
        );
    }

    // ---- resolve_rollback_identity / read_git_identity ----

    #[test]
    #[serial_test::serial(git_env)]
    fn resolve_rollback_identity_inherits_when_repo_has_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_bare_repo(dir); // sets user.name + user.email

        // Clear any inherited GIT_AUTHOR_*/COMMITTER_* env so the resolver
        // falls through to reading the repo config (which IS configured).
        struct EnvGuard(Vec<(&'static str, Option<String>)>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                for (k, v) in &self.0 {
                    match v {
                        // env-ok: restore/clear inside #[serial(git_env)] test; no concurrent reader
                        Some(val) => unsafe { std::env::set_var(k, val) },
                        // env-ok: restore/clear inside #[serial(git_env)] test; no concurrent reader
                        None => unsafe { std::env::remove_var(k) },
                    }
                }
            }
        }
        let keys = [
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
        ];
        let _g = EnvGuard(keys.iter().map(|k| (*k, std::env::var(k).ok())).collect());
        for k in keys {
            // env-ok: restore/clear inside #[serial(git_env)] test; no concurrent reader
            unsafe { std::env::remove_var(k) };
        }

        // Repo has user.name + user.email -> inherit (empty identity).
        let id = resolve_rollback_identity(dir);
        assert!(
            id.name.is_none() && id.email.is_none(),
            "configured repo identity must be inherited, not overridden: {id:?}"
        );
    }

    #[test]
    #[serial_test::serial(git_env)]
    fn resolve_rollback_identity_synthesizes_when_no_identity_anywhere() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // init WITHOUT configuring user.name / user.email.
        g(dir, &["init", "-b", "master"]);

        struct EnvGuard(Vec<(&'static str, Option<String>)>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                for (k, v) in &self.0 {
                    match v {
                        // env-ok: restore/clear inside #[serial(git_env)] test; no concurrent reader
                        Some(val) => unsafe { std::env::set_var(k, val) },
                        // env-ok: restore/clear inside #[serial(git_env)] test; no concurrent reader
                        None => unsafe { std::env::remove_var(k) },
                    }
                }
            }
        }
        let keys = [
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
        ];
        let _g = EnvGuard(keys.iter().map(|k| (*k, std::env::var(k).ok())).collect());
        for k in keys {
            // env-ok: restore/clear inside #[serial(git_env)] test; no concurrent reader
            unsafe { std::env::remove_var(k) };
        }

        // Best-effort: global git config may still supply an identity on the
        // host. Only assert the synthetic path when the repo truly has none.
        let (n, e) = read_git_identity(dir);
        if n.is_none() || e.is_none() {
            let id = resolve_rollback_identity(dir);
            assert_eq!(id.name.as_deref(), Some("anodize-rollback"));
            assert!(
                id.email
                    .as_deref()
                    .unwrap_or("")
                    .starts_with("anodize-rollback@"),
                "synthetic identity required when no config present: {id:?}"
            );
        }
    }

    #[test]
    fn read_git_identity_reads_configured_values() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        g(dir, &["init", "-b", "master"]);
        g(dir, &["config", "user.name", "Configured Name"]);
        g(dir, &["config", "user.email", "configured@x.com"]);

        let (name, email) = read_git_identity(dir);
        assert_eq!(name.as_deref(), Some("Configured Name"));
        assert_eq!(email.as_deref(), Some("configured@x.com"));
    }
}
