use super::*;
use anyhow::{Context as _, Result, bail};
use std::path::Path;
use std::process::Command;

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
/// `ShortCommit` template var populated by [`crate::git::detect_git_info`]
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
