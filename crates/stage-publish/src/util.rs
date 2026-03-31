use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::RepositoryConfig;
use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};
use std::path::Path;
use std::process::Command;

/// Run a command in a specific working directory, failing with `label`
/// on spawn failure or non-zero exit.  Captures stdout/stderr so that
/// diagnostics are included in the error message.
pub(crate) fn run_cmd_in(dir: &Path, program: &str, args: &[&str], label: &str) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("{}: failed to run {} {}", label, program, args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "{}: {} {} failed (exit {})\nstderr: {}\nstdout: {}",
            label,
            program,
            args.join(" "),
            output.status.code().unwrap_or(-1),
            stderr,
            stdout
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Publisher config lookup
// ---------------------------------------------------------------------------

use anodize_core::config::{CrateConfig, PublishConfig};

/// Look up a crate's config and its `publish` section by name, returning a
/// descriptive error when either is missing.
pub(crate) fn get_publish_config<'a>(
    ctx: &'a Context,
    crate_name: &str,
    label: &str,
) -> Result<(&'a CrateConfig, &'a PublishConfig)> {
    let crate_cfg = ctx
        .config
        .crates
        .iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| anyhow::anyhow!("{label}: crate '{crate_name}' not found in config"))?;

    let publish = crate_cfg
        .publish
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{label}: no publish config for '{crate_name}'"))?;

    Ok((crate_cfg, publish))
}

// ---------------------------------------------------------------------------
// Artifact kind resolution
// ---------------------------------------------------------------------------

/// Map the `use` config value (e.g. "archive", "msi", "nsis") to an
/// `ArtifactKind`.  Defaults to `Archive` when the value is `None` or
/// unrecognised.
pub(crate) fn resolve_artifact_kind(use_value: Option<&str>) -> ArtifactKind {
    match use_value {
        Some("msi") | Some("nsis") => ArtifactKind::Installer,
        // "archive" or anything else defaults to Archive
        _ => ArtifactKind::Archive,
    }
}

// ---------------------------------------------------------------------------
// Token resolution
// ---------------------------------------------------------------------------

/// Resolve an auth token from the context, then a publisher-specific env var,
/// then `ANODIZE_GITHUB_TOKEN`, then the generic `GITHUB_TOKEN` env var.
pub(crate) fn resolve_token(ctx: &Context, env_var: Option<&str>) -> Option<String> {
    ctx.options
        .token
        .clone()
        .or_else(|| env_var.and_then(|v| std::env::var(v).ok()))
        .or_else(|| std::env::var("ANODIZE_GITHUB_TOKEN").ok())
        .or_else(|| std::env::var("GITHUB_TOKEN").ok())
}

// ---------------------------------------------------------------------------
// Git repo helpers  (clone, configure auth, commit, push)
// ---------------------------------------------------------------------------

/// Clone a git repo into `tmp_dir` using `http.extraheader` for auth (avoids
/// leaking tokens in URLs).  Also configures auth on the clone for subsequent
/// push operations.
pub(crate) fn clone_repo_with_auth(
    repo_url: &str,
    token: Option<&str>,
    tmp_dir: &Path,
    label: &str,
    log: &StageLogger,
) -> Result<()> {
    let auth_header;
    let mut clone_args: Vec<&str> = vec!["clone", "--depth=1"];
    if let Some(tok) = token {
        auth_header = format!("http.extraheader=Authorization: bearer {}", tok);
        clone_args.extend_from_slice(&["-c", &auth_header]);
    }
    clone_args.push(repo_url);
    let repo_path_str = tmp_dir.to_string_lossy();
    clone_args.push(&repo_path_str);

    let output = Command::new("git")
        .args(&clone_args)
        .output()
        .with_context(|| format!("{label}: git clone: spawn"))?;
    log.check_output(output, &format!("{label}: git clone"))?;

    // Configure auth for subsequent push operations in this repo clone.
    if let Some(tok) = token {
        run_cmd_in(
            tmp_dir,
            "git",
            &[
                "config",
                "http.extraheader",
                &format!("Authorization: bearer {}", tok),
            ],
            &format!("{label}: git config auth"),
        )?;
    }

    Ok(())
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
        let key_path = key_dir.join(".anodize_ssh_key");
        std::fs::write(&key_path, key_content)
            .with_context(|| format!("{label}: write SSH private key"))?;
        // SSH requires the key file to be user-readable only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("{label}: set SSH key permissions"))?;
        }
        let built_ssh_cmd = format!("ssh -i {} -o StrictHostKeyChecking=no", key_path.display());
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
    // Warn when token_type is set to a non-GitHub value, since anodize
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

/// Submit a pull request if `repo.pull_request.enabled` is true.
///
/// Uses `pull_request.base` for the upstream target when available,
/// falling back to `repo_owner/repo_name`.  Supports `pull_request.draft`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn maybe_submit_pr(
    repo_path: &Path,
    repo: Option<&RepositoryConfig>,
    repo_owner: &str,
    repo_name: &str,
    branch_name: &str,
    title: &str,
    body: &str,
    label: &str,
    log: &StageLogger,
) {
    let pr_cfg = match repo.and_then(|r| r.pull_request.as_ref()) {
        Some(pr) if pr.enabled == Some(true) => pr,
        _ => return,
    };

    // Determine the upstream target repo slug.
    let upstream_slug = if let Some(ref base) = pr_cfg.base {
        if let (Some(owner), Some(name)) = (&base.owner, &base.name) {
            format!("{}/{}", owner, name)
        } else {
            format!("{}/{}", repo_owner, repo_name)
        }
    } else {
        format!("{}/{}", repo_owner, repo_name)
    };

    // Build the PR body, preferring the config body if set.
    let pr_body = pr_cfg.body.as_deref().unwrap_or(body);

    // Build head reference: owner:branch.
    let head = format!("{}:{}", repo_owner, branch_name);

    let is_draft = pr_cfg.draft == Some(true);

    // Determine the target base branch for the PR.
    let base_branch = pr_cfg.base.as_ref().and_then(|b| b.branch.as_deref());

    let mut args = vec![
        "pr",
        "create",
        "--repo",
        &upstream_slug,
        "--title",
        title,
        "--body",
        pr_body,
        "--head",
        &head,
    ];

    if let Some(base) = base_branch {
        args.push("--base");
        args.push(base);
    }

    if is_draft {
        args.push("--draft");
    }

    let pr_result = Command::new("gh")
        .current_dir(repo_path)
        .args(&args)
        .output();

    match pr_result {
        Ok(output) if output.status.success() => {
            log.status(&format!("{label}: PR submitted"));
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log.warn(&format!(
                "{label}: gh pr create exited with {} -- you may need to create the PR manually{}",
                output.status,
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", stderr)
                }
            ));
        }
        Err(e) => {
            log.warn(&format!(
                "{label}: could not run gh to create PR: {} -- you may need to create the PR manually",
                e
            ));
        }
    }
}

/// Optional overrides for the git commit step.
#[derive(Default)]
pub(crate) struct CommitOptions<'a> {
    /// Git commit author name (passed via `-c user.name=X`).
    pub author_name: Option<&'a str>,
    /// Git commit author email (passed via `-c user.email=X`).
    pub author_email: Option<&'a str>,
    /// Enable GPG/SSH signing for the commit.
    pub signing: Option<&'a anodize_core::config::CommitSigningConfig>,
}

/// Resolve repository owner/name from a RepositoryConfig, falling back to
/// a legacy config's owner/name pair.
pub(crate) fn resolve_repo_owner_name(
    repo: Option<&anodize_core::config::RepositoryConfig>,
    legacy_owner: Option<&str>,
    legacy_name: Option<&str>,
) -> Option<(String, String)> {
    if let Some(r) = repo
        && let (Some(o), Some(n)) = (r.owner.as_deref(), r.name.as_deref())
    {
        return Some((o.to_string(), n.to_string()));
    }
    if let (Some(o), Some(n)) = (legacy_owner, legacy_name) {
        return Some((o.to_string(), n.to_string()));
    }
    None
}

/// Default commit author name used when no author is configured.
/// Mirrors GoReleaser's default of "goreleaser".
const DEFAULT_COMMIT_AUTHOR_NAME: &str = "anodize";

/// Default commit author email used when no author is configured.
/// Mirrors GoReleaser's default of "goreleaser@carlosbecker.com".
const DEFAULT_COMMIT_AUTHOR_EMAIL: &str = "bot@anodize.dev";

/// Resolve commit author name/email from a CommitAuthorConfig, falling back
/// to legacy per-publisher fields, then to built-in defaults.
pub(crate) fn resolve_commit_opts<'a>(
    commit_author: Option<&'a anodize_core::config::CommitAuthorConfig>,
    legacy_name: Option<&'a str>,
    legacy_email: Option<&'a str>,
) -> CommitOptions<'a> {
    let (name, email, signing) = if let Some(ca) = commit_author {
        (
            ca.name.as_deref().or(legacy_name),
            ca.email.as_deref().or(legacy_email),
            ca.signing.as_ref(),
        )
    } else {
        (legacy_name, legacy_email, None)
    };
    CommitOptions {
        author_name: Some(name.unwrap_or(DEFAULT_COMMIT_AUTHOR_NAME)),
        author_email: Some(email.unwrap_or(DEFAULT_COMMIT_AUTHOR_EMAIL)),
        signing,
    }
}

/// Resolve the repository token from: RepositoryConfig.token → env_var → ANODIZE_GITHUB_TOKEN → GITHUB_TOKEN.
pub(crate) fn resolve_repo_token(
    ctx: &Context,
    repo: Option<&anodize_core::config::RepositoryConfig>,
    env_var: Option<&str>,
) -> Option<String> {
    // 1. Token from repository config
    if let Some(r) = repo
        && let Some(ref tok) = r.token
        && !tok.is_empty()
    {
        return Some(tok.clone());
    }
    // 2. Fall back to context + env
    resolve_token(ctx, env_var)
}

/// Resolve the branch to push to from RepositoryConfig.
pub(crate) fn resolve_branch(
    repo: Option<&anodize_core::config::RepositoryConfig>,
) -> Option<&str> {
    repo.and_then(|r| r.branch.as_deref())
}

/// Stage files, commit, and push with optional commit author overrides.
pub(crate) fn commit_and_push_with_opts(
    repo_path: &Path,
    files: &[&str],
    message: &str,
    branch: Option<&str>,
    label: &str,
    opts: &CommitOptions<'_>,
) -> Result<()> {
    if let Some(branch_name) = branch {
        run_cmd_in(
            repo_path,
            "git",
            &["checkout", "-b", branch_name],
            &format!("{label}: git checkout"),
        )?;
    }

    for file in files {
        run_cmd_in(
            repo_path,
            "git",
            &["add", file],
            &format!("{label}: git add"),
        )?;
    }

    // Build commit args, optionally injecting -c user.name / -c user.email / signing.
    let mut commit_args: Vec<&str> = Vec::new();
    let name_cfg;
    let email_cfg;
    let sign_cfg;
    let sign_key_cfg;
    let sign_program_cfg;
    let sign_format_cfg;
    if let Some(name) = opts.author_name {
        name_cfg = format!("user.name={}", name);
        commit_args.extend_from_slice(&["-c", &name_cfg]);
    }
    if let Some(email) = opts.author_email {
        email_cfg = format!("user.email={}", email);
        commit_args.extend_from_slice(&["-c", &email_cfg]);
    }
    // Handle commit signing config
    let do_sign = opts.signing.and_then(|s| s.enabled).unwrap_or(false);
    if do_sign {
        sign_cfg = "commit.gpgsign=true".to_string();
        commit_args.extend_from_slice(&["-c", &sign_cfg]);
        if let Some(key) = opts.signing.and_then(|s| s.key.as_deref()) {
            sign_key_cfg = format!("user.signingkey={}", key);
            commit_args.extend_from_slice(&["-c", &sign_key_cfg]);
        }
        if let Some(program) = opts.signing.and_then(|s| s.program.as_deref()) {
            sign_program_cfg = format!("gpg.program={}", program);
            commit_args.extend_from_slice(&["-c", &sign_program_cfg]);
        }
        if let Some(fmt) = opts.signing.and_then(|s| s.format.as_deref()) {
            sign_format_cfg = format!("gpg.format={}", fmt);
            commit_args.extend_from_slice(&["-c", &sign_format_cfg]);
        }
    }
    commit_args.extend_from_slice(&["commit", "-m", message]);

    run_cmd_in(
        repo_path,
        "git",
        &commit_args,
        &format!("{label}: git commit"),
    )?;

    let push_args: Vec<&str> = if let Some(branch_name) = branch {
        vec!["push", "-u", "origin", branch_name]
    } else {
        vec!["push"]
    };

    run_cmd_in(repo_path, "git", &push_args, &format!("{label}: git push"))
}

// ---------------------------------------------------------------------------
// PR submission via `gh` CLI
// ---------------------------------------------------------------------------

/// Submit a pull request via the GitHub CLI. Logs a warning instead of failing
/// if `gh` is not available or the command exits non-zero.
pub(crate) fn submit_pr_via_gh(
    repo_path: &Path,
    upstream_repo: &str,
    head: &str,
    title: &str,
    body: &str,
    label: &str,
    log: &StageLogger,
) {
    let pr_result = Command::new("gh")
        .current_dir(repo_path)
        .args([
            "pr",
            "create",
            "--repo",
            upstream_repo,
            "--title",
            title,
            "--body",
            body,
            "--head",
            head,
        ])
        .output();

    match pr_result {
        Ok(output) if output.status.success() => {
            log.status(&format!("{label}: PR submitted"));
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log.warn(&format!(
                "{label}: gh pr create exited with {} — you may need to create the PR manually{}",
                output.status,
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", stderr)
                }
            ));
        }
        Err(e) => {
            log.warn(&format!(
                "{label}: could not run gh to create PR: {} — you may need to create the PR manually",
                e
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Windows artifact helper
// ---------------------------------------------------------------------------

/// Find a Windows Archive artifact and return `(url, sha256)`, or bail with a
/// descriptive error.
#[allow(dead_code)]
pub(crate) fn require_windows_artifact(
    ctx: &Context,
    crate_name: &str,
    label: &str,
) -> Result<(String, String)> {
    find_windows_artifact(ctx, crate_name).ok_or_else(|| {
        anyhow::anyhow!(
            "{}: no Windows archive artifact found for crate '{}'",
            label,
            crate_name
        )
    })
}

// ---------------------------------------------------------------------------
// YAML quoting (shared by winget, krew, and any other YAML-producing publisher)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// OS / architecture inference from target triples
// ---------------------------------------------------------------------------
//
// The functions below provide a two-layer normalisation scheme:
//
// 1. **Generic inference** (`infer_os` / `infer_arch`):
//    Map a Rust-style target triple (e.g. `x86_64-unknown-linux-gnu`,
//    `aarch64-apple-darwin`) to a canonical short form used internally
//    by `OsArtifact` (`"linux"`, `"darwin"`, `"windows"`, `"amd64"`,
//    `"arm64"`).
//
// 2. **Publisher-specific mapping** (e.g. `krew_os`, `krew_arch` in krew.rs):
//    Translate the canonical form to whatever the target ecosystem expects.
//    For Krew the mapping is effectively a no-op today, but keeping a
//    separate layer means we can adjust for future drift without touching
//    the shared inference code.
//
// Both `find_artifacts_by_os` and `find_all_platform_artifacts` use these
// shared helpers so the inference logic lives in exactly one place.

/// Infer the canonical OS string from a target triple.
///
/// Delegates to [`anodize_core::target::map_target`] for the actual parsing.
/// Returns the mapped OS, or `fallback` when the OS is `"unknown"`.
pub(crate) fn infer_os(target: &str, fallback: &str) -> String {
    let (os, _) = anodize_core::target::map_target(target);
    if os == "unknown" {
        fallback.to_string()
    } else {
        os
    }
}

/// Infer the canonical architecture string from a target triple.
///
/// Delegates to [`anodize_core::target::map_target`] for the actual parsing.
pub(crate) fn infer_arch(target: &str) -> String {
    let (_, arch) = anodize_core::target::map_target(target);
    arch
}

/// Describes the OS + architecture of an artifact match.
pub(crate) struct OsArtifact {
    pub url: String,
    pub sha256: String,
    pub os: String,
    pub arch: String,
    #[allow(dead_code)]
    pub id: Option<String>,
}

/// Convert a single `Artifact` reference into an `OsArtifact`, using the
/// shared `infer_os` / `infer_arch` helpers.
///
/// `os_fallback` is used when the OS cannot be determined from the target
/// triple (e.g. when calling from `find_artifacts_by_os` with a known needle).
fn artifact_to_os_artifact(a: &Artifact, os_fallback: &str) -> OsArtifact {
    let url = a
        .metadata
        .get("url")
        .cloned()
        .unwrap_or_else(|| a.path.to_string_lossy().into_owned());
    let sha256 = a.metadata.get("sha256").cloned().unwrap_or_default();
    let id = a.metadata.get("id").cloned();
    let target = a.target.as_deref().unwrap_or("");
    OsArtifact {
        url,
        sha256,
        os: infer_os(target, os_fallback),
        arch: infer_arch(target),
        id,
    }
}

/// Filter a vec of `OsArtifact` by IDs: when `ids` is `Some`, keep only
/// artifacts whose `id` field matches one of the given IDs.  When `ids` is
/// `None`, all artifacts pass through.
#[allow(dead_code)]
pub(crate) fn filter_os_artifacts_by_ids(
    artifacts: Vec<OsArtifact>,
    ids: Option<&[String]>,
) -> Vec<OsArtifact> {
    if let Some(ids) = ids {
        artifacts
            .into_iter()
            .filter(|a| {
                a.id.as_ref()
                    .map(|id| ids.iter().any(|i| i == id))
                    .unwrap_or(false)
            })
            .collect()
    } else {
        artifacts
    }
}

/// Filter artifacts by IDs: when `ids` is `Some`, keep only artifacts whose
/// metadata `"id"` key matches one of the given IDs.  When `ids` is `None`,
/// all artifacts pass through.
pub(crate) fn filter_by_ids<'a>(
    artifacts: Vec<&'a Artifact>,
    ids: Option<&[String]>,
) -> Vec<&'a Artifact> {
    if let Some(ids) = ids {
        artifacts
            .into_iter()
            .filter(|a| {
                a.metadata
                    .get("id")
                    .map(|id| ids.iter().any(|i| i == id))
                    .unwrap_or(false)
            })
            .collect()
    } else {
        artifacts
    }
}

/// Render a `url_template` string with Tera, providing `name`, `version`,
/// `arch`, and `os` variables.  Returns the rendered URL.
pub(crate) fn render_url_template(
    template: &str,
    name: &str,
    version: &str,
    arch: &str,
    os: &str,
) -> String {
    let mut tera = tera::Tera::default();
    tera.autoescape_on(vec![]);
    if tera.add_raw_template("url", template).is_err() {
        return template.to_string();
    }
    let mut ctx = tera::Context::new();
    ctx.insert("name", name);
    ctx.insert("version", version);
    ctx.insert("arch", arch);
    ctx.insert("os", os);
    tera.render("url", &ctx)
        .unwrap_or_else(|_| template.to_string())
}

/// Find all Archive artifacts for the given crate whose target or path
/// matches `os_needle` (e.g. "linux", "darwin", "windows").
///
/// Returns a vec of `OsArtifact` with the URL, SHA256, and inferred
/// os/arch strings extracted from the target triple.
#[allow(dead_code)]
pub(crate) fn find_artifacts_by_os(
    ctx: &Context,
    crate_name: &str,
    os_needle: &str,
) -> Vec<OsArtifact> {
    find_artifacts_by_os_filtered(ctx, crate_name, os_needle, None)
}

/// Find all Archive artifacts for the given crate whose target or path
/// matches `os_needle`, with optional IDs filter.
pub(crate) fn find_artifacts_by_os_filtered(
    ctx: &Context,
    crate_name: &str,
    os_needle: &str,
    ids: Option<&[String]>,
) -> Vec<OsArtifact> {
    // Include both Archive and Binary artifacts — GoReleaser supports both
    // UploadableArchive and UploadableBinary types for publisher packages.
    let mut all = ctx
        .artifacts
        .by_kind_and_crate(ArtifactKind::Archive, crate_name);
    all.extend(
        ctx.artifacts
            .by_kind_and_crate(ArtifactKind::Binary, crate_name),
    );
    let filtered = filter_by_ids(all, ids);
    filtered
        .into_iter()
        .filter(|a| {
            a.target
                .as_deref()
                .map(|t| t.to_ascii_lowercase().contains(os_needle))
                .unwrap_or(false)
                || a.path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains(os_needle)
        })
        .map(|a| artifact_to_os_artifact(a, os_needle))
        .collect()
}

/// Find all Archive artifacts for the given crate across all platforms.
///
/// Returns a vec of `OsArtifact` with the URL, SHA256, and inferred
/// os/arch strings extracted from the target triple.
#[allow(dead_code)]
pub(crate) fn find_all_platform_artifacts(ctx: &Context, crate_name: &str) -> Vec<OsArtifact> {
    find_all_platform_artifacts_filtered(ctx, crate_name, None)
}

/// Find all Archive and Binary artifacts for the given crate across all platforms,
/// with optional IDs filter.
pub(crate) fn find_all_platform_artifacts_filtered(
    ctx: &Context,
    crate_name: &str,
    ids: Option<&[String]>,
) -> Vec<OsArtifact> {
    let mut all = ctx
        .artifacts
        .by_kind_and_crate(ArtifactKind::Archive, crate_name);
    all.extend(
        ctx.artifacts
            .by_kind_and_crate(ArtifactKind::Binary, crate_name),
    );
    let filtered = filter_by_ids(all, ids);
    filtered
        .into_iter()
        .map(|a| artifact_to_os_artifact(a, "unknown"))
        .collect()
}

/// Find a Windows Archive artifact for the given crate and return `(url, sha256)`.
///
/// Returns `None` when no matching artifact exists.
#[allow(dead_code)]
pub(crate) fn find_windows_artifact(ctx: &Context, crate_name: &str) -> Option<(String, String)> {
    let a = find_artifacts_by_os(ctx, crate_name, "windows")
        .into_iter()
        .next()?;
    Some((a.url, a.sha256))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodize_core::artifact::{Artifact, ArtifactKind};
    use anodize_core::config::{Config, CrateConfig};
    use anodize_core::context::{Context, ContextOptions};
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// Helper: build a Context with mock Archive artifacts for a given crate.
    fn ctx_with_artifacts(crate_name: &str, artifacts: Vec<(&str, &str, &str)>) -> Context {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: crate_name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];
        let mut ctx = Context::new(config, ContextOptions::default());
        for (target, url, sha256) in artifacts {
            let mut meta = HashMap::new();
            meta.insert("url".to_string(), url.to_string());
            meta.insert("sha256".to_string(), sha256.to_string());
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Archive,
                name: String::new(),
                path: PathBuf::from(format!(
                    "dist/{}",
                    url.rsplit('/').next().unwrap_or("a.tar.gz")
                )),
                target: Some(target.to_string()),
                crate_name: crate_name.to_string(),
                metadata: meta,
            });
        }
        ctx
    }

    // -----------------------------------------------------------------------
    // infer_os / infer_arch unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_infer_os_linux() {
        assert_eq!(infer_os("x86_64-unknown-linux-gnu", "fallback"), "linux");
        assert_eq!(infer_os("aarch64-unknown-linux-musl", "fallback"), "linux");
    }

    #[test]
    fn test_infer_os_darwin() {
        assert_eq!(infer_os("aarch64-apple-darwin", "fallback"), "darwin");
        assert_eq!(infer_os("x86_64-apple-darwin", "fallback"), "darwin");
    }

    #[test]
    fn test_infer_os_windows() {
        assert_eq!(infer_os("x86_64-pc-windows-msvc", "fallback"), "windows");
    }

    #[test]
    fn test_infer_os_unknown_uses_fallback() {
        assert_eq!(
            infer_os("wasm32-unknown-unknown", "myfallback"),
            "myfallback"
        );
    }

    #[test]
    fn test_infer_arch_x86_64() {
        assert_eq!(infer_arch("x86_64-unknown-linux-gnu"), "amd64");
        assert_eq!(infer_arch("x86_64-pc-windows-msvc"), "amd64");
        assert_eq!(infer_arch("x86_64-apple-darwin"), "amd64");
    }

    #[test]
    fn test_infer_arch_aarch64() {
        assert_eq!(infer_arch("aarch64-apple-darwin"), "arm64");
        assert_eq!(infer_arch("aarch64-unknown-linux-musl"), "arm64");
    }

    #[test]
    fn test_infer_arch_unknown() {
        // map_target passes unrecognised arch prefixes through verbatim
        assert_eq!(infer_arch("wasm32-unknown-unknown"), "wasm32");
    }

    // -----------------------------------------------------------------------
    // find_artifacts_by_os tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_find_artifacts_by_os_linux() {
        let ctx = ctx_with_artifacts(
            "mytool",
            vec![
                (
                    "x86_64-unknown-linux-gnu",
                    "https://example.com/mytool-linux-amd64.tar.gz",
                    "hash_linux_amd64",
                ),
                (
                    "aarch64-unknown-linux-musl",
                    "https://example.com/mytool-linux-arm64.tar.gz",
                    "hash_linux_arm64",
                ),
                (
                    "aarch64-apple-darwin",
                    "https://example.com/mytool-darwin-arm64.tar.gz",
                    "hash_darwin_arm64",
                ),
                (
                    "x86_64-pc-windows-msvc",
                    "https://example.com/mytool-windows-amd64.zip",
                    "hash_win_amd64",
                ),
            ],
        );

        let linux = find_artifacts_by_os(&ctx, "mytool", "linux");
        assert_eq!(linux.len(), 2);
        assert!(linux.iter().all(|a| a.os == "linux"));
        assert!(
            linux
                .iter()
                .any(|a| a.arch == "amd64" && a.sha256 == "hash_linux_amd64")
        );
        assert!(
            linux
                .iter()
                .any(|a| a.arch == "arm64" && a.sha256 == "hash_linux_arm64")
        );
    }

    #[test]
    fn test_find_artifacts_by_os_darwin() {
        let ctx = ctx_with_artifacts(
            "mytool",
            vec![
                (
                    "x86_64-unknown-linux-gnu",
                    "https://example.com/mytool-linux-amd64.tar.gz",
                    "h1",
                ),
                (
                    "aarch64-apple-darwin",
                    "https://example.com/mytool-darwin-arm64.tar.gz",
                    "h2",
                ),
                (
                    "x86_64-apple-darwin",
                    "https://example.com/mytool-darwin-amd64.tar.gz",
                    "h3",
                ),
            ],
        );

        let darwin = find_artifacts_by_os(&ctx, "mytool", "darwin");
        assert_eq!(darwin.len(), 2);
        assert!(darwin.iter().all(|a| a.os == "darwin"));
    }

    #[test]
    fn test_find_artifacts_by_os_no_match() {
        let ctx = ctx_with_artifacts(
            "mytool",
            vec![(
                "x86_64-unknown-linux-gnu",
                "https://example.com/mytool-linux-amd64.tar.gz",
                "h1",
            )],
        );

        let windows = find_artifacts_by_os(&ctx, "mytool", "windows");
        assert!(windows.is_empty());
    }

    // -----------------------------------------------------------------------
    // find_all_platform_artifacts tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_find_all_platform_artifacts() {
        let ctx = ctx_with_artifacts(
            "mytool",
            vec![
                (
                    "x86_64-unknown-linux-gnu",
                    "https://example.com/linux-amd64.tar.gz",
                    "h1",
                ),
                (
                    "aarch64-apple-darwin",
                    "https://example.com/darwin-arm64.tar.gz",
                    "h2",
                ),
                (
                    "x86_64-pc-windows-msvc",
                    "https://example.com/windows-amd64.zip",
                    "h3",
                ),
            ],
        );

        let all = find_all_platform_artifacts(&ctx, "mytool");
        assert_eq!(all.len(), 3);
        assert!(all.iter().any(|a| a.os == "linux" && a.arch == "amd64"));
        assert!(all.iter().any(|a| a.os == "darwin" && a.arch == "arm64"));
        assert!(all.iter().any(|a| a.os == "windows" && a.arch == "amd64"));
    }

    #[test]
    fn test_find_all_platform_artifacts_empty() {
        let ctx = ctx_with_artifacts("mytool", vec![]);
        let all = find_all_platform_artifacts(&ctx, "mytool");
        assert!(all.is_empty());
    }

    #[test]
    fn test_find_all_platform_artifacts_wrong_crate() {
        let ctx = ctx_with_artifacts(
            "mytool",
            vec![(
                "x86_64-unknown-linux-gnu",
                "https://example.com/linux-amd64.tar.gz",
                "h1",
            )],
        );
        let all = find_all_platform_artifacts(&ctx, "other_tool");
        assert!(all.is_empty());
    }

    // -----------------------------------------------------------------------
    // OsArtifact id field tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_os_artifact_has_id_from_metadata() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];
        let mut ctx = Context::new(config, ContextOptions::default());
        let mut meta = HashMap::new();
        meta.insert(
            "url".to_string(),
            "https://example.com/a.tar.gz".to_string(),
        );
        meta.insert("sha256".to_string(), "abc".to_string());
        meta.insert("id".to_string(), "my-archive".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/a.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "mytool".to_string(),
            metadata: meta,
        });

        let all = find_all_platform_artifacts(&ctx, "mytool");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id.as_deref(), Some("my-archive"));
    }

    #[test]
    fn test_os_artifact_id_is_none_when_not_in_metadata() {
        let ctx = ctx_with_artifacts(
            "mytool",
            vec![(
                "x86_64-unknown-linux-gnu",
                "https://example.com/a.tar.gz",
                "abc",
            )],
        );
        let all = find_all_platform_artifacts(&ctx, "mytool");
        assert_eq!(all.len(), 1);
        assert!(all[0].id.is_none());
    }

    // -----------------------------------------------------------------------
    // filter_os_artifacts_by_ids tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_filter_os_artifacts_by_ids_none_passes_all() {
        let artifacts = vec![
            OsArtifact {
                url: "u1".to_string(),
                sha256: "s1".to_string(),
                os: "linux".to_string(),
                arch: "amd64".to_string(),
                id: Some("a".to_string()),
            },
            OsArtifact {
                url: "u2".to_string(),
                sha256: "s2".to_string(),
                os: "darwin".to_string(),
                arch: "arm64".to_string(),
                id: Some("b".to_string()),
            },
        ];
        let result = filter_os_artifacts_by_ids(artifacts, None);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_filter_os_artifacts_by_ids_filters_matching() {
        let artifacts = vec![
            OsArtifact {
                url: "u1".to_string(),
                sha256: "s1".to_string(),
                os: "linux".to_string(),
                arch: "amd64".to_string(),
                id: Some("keep-me".to_string()),
            },
            OsArtifact {
                url: "u2".to_string(),
                sha256: "s2".to_string(),
                os: "darwin".to_string(),
                arch: "arm64".to_string(),
                id: Some("drop-me".to_string()),
            },
            OsArtifact {
                url: "u3".to_string(),
                sha256: "s3".to_string(),
                os: "windows".to_string(),
                arch: "amd64".to_string(),
                id: None,
            },
        ];
        let ids = vec!["keep-me".to_string()];
        let result = filter_os_artifacts_by_ids(artifacts, Some(&ids));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].url, "u1");
    }

    #[test]
    fn test_filter_os_artifacts_by_ids_empty_ids_returns_nothing() {
        let artifacts = vec![OsArtifact {
            url: "u1".to_string(),
            sha256: "s1".to_string(),
            os: "linux".to_string(),
            arch: "amd64".to_string(),
            id: Some("a".to_string()),
        }];
        let ids: Vec<String> = vec![];
        let result = filter_os_artifacts_by_ids(artifacts, Some(&ids));
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // resolve_artifact_kind tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_artifact_kind_none_defaults_to_archive() {
        assert!(matches!(resolve_artifact_kind(None), ArtifactKind::Archive));
    }

    #[test]
    fn test_resolve_artifact_kind_archive() {
        assert!(matches!(
            resolve_artifact_kind(Some("archive")),
            ArtifactKind::Archive
        ));
    }

    #[test]
    fn test_resolve_artifact_kind_msi() {
        assert!(matches!(
            resolve_artifact_kind(Some("msi")),
            ArtifactKind::Installer
        ));
    }

    #[test]
    fn test_resolve_artifact_kind_nsis() {
        assert!(matches!(
            resolve_artifact_kind(Some("nsis")),
            ArtifactKind::Installer
        ));
    }

    #[test]
    fn test_resolve_artifact_kind_unknown_defaults_to_archive() {
        assert!(matches!(
            resolve_artifact_kind(Some("unknown")),
            ArtifactKind::Archive
        ));
    }

    // -----------------------------------------------------------------------
    // render_url_template tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_render_url_template_basic() {
        let url = render_url_template(
            "https://example.com/{{ name }}/{{ version }}/{{ arch }}-{{ os }}.zip",
            "mytool",
            "1.2.3",
            "amd64",
            "windows",
        );
        assert_eq!(url, "https://example.com/mytool/1.2.3/amd64-windows.zip");
    }

    #[test]
    fn test_render_url_template_invalid_fallback() {
        let url = render_url_template(
            "https://example.com/{{ bad unclosed",
            "mytool",
            "1.0.0",
            "amd64",
            "linux",
        );
        assert_eq!(url, "https://example.com/{{ bad unclosed");
    }
}
