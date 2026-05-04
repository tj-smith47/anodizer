//! Repository cloning helpers for publishers — HTTPS (token-based),
//! SSH (private-key or `GIT_SSH_COMMAND`), and the `clone_repo` smart
//! dispatcher that picks one based on `RepositoryConfig`.

use anodizer_core::config::RepositoryConfig;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};
use std::path::Path;
use std::process::Command;

use super::cmd::run_cmd_in;

/// Clone a git repo into `tmp_dir` with token-based auth.
///
/// Uses a git credential helper that injects the token, which is more
/// reliable than `http.extraheader` across different GitHub token types
/// (classic PATs, fine-grained PATs, GitHub App tokens, GITHUB_TOKEN).
/// The `http.extraheader=Authorization: bearer` approach can be overridden
/// by system credential helpers and doesn't work with all token types.
pub(crate) fn clone_repo_with_auth(
    repo_url: &str,
    token: Option<&str>,
    tmp_dir: &Path,
    label: &str,
    log: &StageLogger,
) -> Result<()> {
    // Embed token in the URL for the clone operation.  This is the same
    // approach used by actions/checkout and is reliable for all GitHub
    // token types.  The URL is only used locally in the subprocess.
    let effective_url = if let Some(tok) = token {
        inject_token_in_url(repo_url, tok)
    } else {
        repo_url.to_string()
    };

    let clone_args: Vec<&str> = vec!["clone", "--depth=1", &effective_url];
    let repo_path_str = tmp_dir.to_string_lossy();

    let output = Command::new("git")
        .args(&clone_args)
        .arg(repo_path_str.as_ref())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("{label}: git clone: spawn"))?;
    log.check_output(output, &format!("{label}: git clone"))?;

    // Configure the remote URL for subsequent push operations with auth.
    if let Some(tok) = token {
        let push_url = inject_token_in_url(repo_url, tok);
        run_cmd_in(
            tmp_dir,
            "git",
            &["remote", "set-url", "origin", &push_url],
            &format!("{label}: git set push URL"),
        )?;
    }

    Ok(())
}

/// Inject an auth token into an HTTPS git URL.
/// `https://github.com/owner/repo.git` → `https://x-access-token:<token>@github.com/owner/repo.git`
fn inject_token_in_url(url: &str, token: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://") {
        format!("https://x-access-token:{}@{}", token, rest)
    } else {
        url.to_string()
    }
}

/// Clone a git repo via SSH, optionally using a private key file or custom
/// SSH command.  When `private_key` is set, it is written to a temporary
/// file and referenced via `GIT_SSH_COMMAND`.  When `ssh_command` is set
/// directly, it takes precedence.
pub(crate) fn clone_repo_ssh(
    git_url: &str,
    private_key: Option<&str>,
    ssh_command: Option<&str>,
    tmp_dir: &Path,
    label: &str,
    log: &StageLogger,
) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.args(["clone", "--depth=1", git_url]);
    let repo_path_str = tmp_dir.to_string_lossy();
    cmd.arg(repo_path_str.as_ref());

    // Determine the GIT_SSH_COMMAND to use.
    // We may need to persist the SSH command string for configuring the repo
    // after clone, so track it here.
    let mut ssh_cmd_for_config: Option<String> = None;

    if let Some(ssh_cmd) = ssh_command {
        // Explicit ssh_command takes precedence.
        cmd.env("GIT_SSH_COMMAND", ssh_cmd);
        ssh_cmd_for_config = Some(ssh_cmd.to_string());
    } else if let Some(key_content) = private_key {
        // Write the private key to a file inside the clone target directory's
        // parent so it lives as long as the caller's tempdir.  We use a
        // sibling directory to avoid conflicts with the clone itself.
        let key_dir = tmp_dir.parent().unwrap_or(tmp_dir);
        let key_path = key_dir.join(".anodizer_ssh_key");
        std::fs::write(&key_path, key_content)
            .with_context(|| format!("{label}: write SSH private key"))?;
        // SSH requires the key file to be user-readable only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("{label}: set SSH key permissions"))?;
        }
        let built_ssh_cmd = format!(
            "ssh -i {} -o StrictHostKeyChecking=accept-new -F /dev/null",
            key_path.display()
        );
        cmd.env("GIT_SSH_COMMAND", &built_ssh_cmd);
        ssh_cmd_for_config = Some(built_ssh_cmd);
    }

    let output = cmd
        .output()
        .with_context(|| format!("{label}: git clone via SSH: spawn"))?;
    log.check_output(output, &format!("{label}: git clone (SSH)"))?;

    // Configure core.sshCommand in the cloned repo so that subsequent push
    // operations use the same SSH credentials.
    if let Some(ref ssh_cfg) = ssh_cmd_for_config {
        run_cmd_in(
            tmp_dir,
            "git",
            &["config", "core.sshCommand", ssh_cfg],
            &format!("{label}: git config sshCommand"),
        )?;
    }

    Ok(())
}

/// Smart clone: decide between HTTPS and SSH based on RepositoryConfig.
///
/// When `repo.git.url` is set, uses SSH-based cloning with optional
/// `private_key` / `ssh_command`.  Otherwise falls back to HTTPS via
/// `clone_repo_with_auth`.
pub(crate) fn clone_repo(
    repo: Option<&RepositoryConfig>,
    fallback_owner: &str,
    fallback_name: &str,
    token: Option<&str>,
    tmp_dir: &Path,
    label: &str,
    log: &StageLogger,
) -> Result<()> {
    // Warn when token_type is set to a non-GitHub value, since anodizer
    // currently only supports GitHub-based repository publishing.
    if let Some(r) = repo
        && let Some(ref tt) = r.token_type
    {
        let tt_lower = tt.to_lowercase();
        if tt_lower != "github" && !tt_lower.is_empty() {
            log.warn(&format!(
                    "{label}: repository.token_type={tt:?} is not yet supported; only \"github\" is currently implemented"
                ));
        }
    }

    // Check if RepositoryConfig specifies a Git SSH URL.
    if let Some(r) = repo
        && let Some(ref git) = r.git
        && let Some(ref url) = git.url
    {
        return clone_repo_ssh(
            url,
            git.private_key.as_deref(),
            git.ssh_command.as_deref(),
            tmp_dir,
            label,
            log,
        );
    }

    // Fall back to HTTPS clone.
    let repo_url = format!(
        "https://github.com/{}/{}.git",
        fallback_owner, fallback_name
    );
    clone_repo_with_auth(&repo_url, token, tmp_dir, label, log)
}
