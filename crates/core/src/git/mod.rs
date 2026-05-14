use anyhow::{Result, bail};
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
    Commit, add_path_in, commit_in, get_all_commits, get_all_commits_paths,
    get_commit_messages_between, get_commit_messages_between_path, get_commits_between,
    get_commits_between_paths, get_current_branch, get_head_commit, get_last_commit_messages,
    get_last_commit_messages_path, get_short_commit, has_changes_since, has_commits_since_tag,
    log_subjects_for_range, paths_changed_since_tag, stage_and_commit,
};
pub use detect::{GitInfo, detect_git_info};
pub use github_api::{create_tag_via_github_api, gh_api_get, gh_api_get_paginated};
pub use remote::{
    detect_github_repo, detect_owner_repo, parse_github_remote, parse_remote_owner_repo,
};
pub use semver::{SemVer, parse_semver, parse_semver_tag};
pub use snapshot_sde::resolve_snapshot_sde;
pub use status::{
    check_git_available, git_status_porcelain, is_git_dirty, is_git_repo, is_shallow_clone,
    local_git_user_email, local_git_user_name,
};
pub use tags::{
    create_and_push_tag, extract_tag_prefix, find_latest_tag_matching,
    find_latest_tag_matching_with_prefix, find_previous_tag, find_previous_tag_with_prefix,
    get_all_semver_tags, get_branch_semver_tags, get_first_commit, has_version_placeholder,
    list_tags_with_prefix, render_ignore_patterns, strip_monorepo_prefix, tag_points_at_head,
};
pub use worktree::Worktree;

/// Run a git command and return stdout, trimmed.
///
/// Shared low-level wrapper used by every submodule. Private to the `git`
/// module — children automatically see private parent items.
///
/// On non-zero exit the stderr is passed through
/// [`crate::redact::redact_process_env`] before interpolation, so any
/// token-bearing remote URL git might echo (e.g.
/// `https://ghp_xxx@github.com/...` produced by an `extraHeader` config
/// leak) is scrubbed in the bail message.
fn git_output(args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).output()?;
    if !output.status.success() {
        let stderr_raw = String::from_utf8_lossy(&output.stderr);
        let raw = format!("git {} failed: {}", args.join(" "), stderr_raw.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
