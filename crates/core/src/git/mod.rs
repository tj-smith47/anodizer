use anyhow::{Result, bail};
use std::path::Path;
use std::process::Command;

mod commits;
mod detect;
mod github_api;
mod remote;
mod semver;
mod snapshot_sde;
mod status;
mod tags;
pub mod worktree;

#[cfg(test)]
mod tests;

pub use commits::{
    Commit, SHORT_COMMIT_LEN, add_path_in, commit_in, get_all_commits, get_all_commits_in,
    get_all_commits_paths, get_all_commits_paths_in, get_commit_messages_between,
    get_commit_messages_between_in, get_commit_messages_between_path,
    get_commit_messages_between_path_in, get_commits_between, get_commits_between_in,
    get_commits_between_paths, get_commits_between_paths_in, get_current_branch,
    get_current_branch_in, get_head_commit, get_head_commit_in, get_last_commit_messages,
    get_last_commit_messages_in, get_last_commit_messages_path, get_last_commit_messages_path_in,
    get_short_commit, get_short_commit_in, has_changes_since, has_changes_since_in,
    has_commits_since_tag, has_commits_since_tag_in, head_commit_hash_in, head_commit_timestamp_in,
    log_subjects_for_range, paths_changed_since_tag, paths_changed_since_tag_in, short_commit_str,
    stage_and_commit, stage_and_commit_in,
};
pub use detect::{GitInfo, detect_git_info, detect_git_info_in};
pub use github_api::{
    create_tag_via_github_api, create_tag_via_github_api_in, gh_api_get, gh_api_get_paginated,
    gh_api_get_paginated_with_binary, gh_api_get_with_binary,
};
pub use remote::{
    detect_github_repo, detect_github_repo_in, detect_owner_repo, detect_owner_repo_in,
    parse_github_remote, parse_remote_owner_repo,
};
pub use semver::{SemVer, parse_semver, parse_semver_tag};
pub use snapshot_sde::resolve_snapshot_sde;
pub use status::{
    check_git_available, git_status_porcelain, git_status_porcelain_in, is_git_dirty,
    is_git_dirty_in, is_git_repo, is_git_repo_in, is_shallow_clone, is_shallow_clone_in,
    local_git_user_email, local_git_user_email_in, local_git_user_name, local_git_user_name_in,
};
pub use tags::{
    create_and_push_tag, create_and_push_tag_in, create_tag_local_only, extract_tag_prefix,
    find_latest_tag_matching, find_latest_tag_matching_in, find_latest_tag_matching_with_prefix,
    find_latest_tag_matching_with_prefix_in, find_previous_tag, find_previous_tag_in,
    find_previous_tag_with_prefix, find_previous_tag_with_prefix_in, get_all_semver_tags,
    get_all_semver_tags_in, get_branch_semver_tags, get_branch_semver_tags_in, get_first_commit,
    get_first_commit_in, get_tags_at_head, get_tags_at_head_in, has_version_placeholder,
    head_is_at_tag, list_tags_with_prefix, push_branch_and_tags_atomic,
    push_branch_and_tags_atomic_in, render_ignore_patterns, strip_monorepo_prefix,
    tag_points_at_head, tag_points_at_head_in,
};
pub use worktree::Worktree;

/// Run `git` in `cwd` and return stdout, trimmed.
///
/// Shared low-level git invocation wrapper. Path-taking so callers
/// that don't own the process cwd — notably tests against a `tempfile::tempdir()`
/// fixture repo — can drive git without mutating the process-wide cwd
/// (which would race every other parallel test). The no-arg public wrappers
/// in sibling submodules delegate here with `std::env::current_dir()`.
///
/// On non-zero exit the stderr is passed through
/// [`crate::redact::redact_process_env`] before interpolation, so any
/// token-bearing remote URL git might echo (e.g.
/// `https://ghp_xxx@github.com/...` produced by an `extraHeader` config
/// leak) is scrubbed in the bail message.
pub(crate) fn git_output_in(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").current_dir(cwd).args(args).output()?;
    if !output.status.success() {
        let stderr_raw = String::from_utf8_lossy(&output.stderr);
        let raw = format!("git {} failed: {}", args.join(" "), stderr_raw.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
