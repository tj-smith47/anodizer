//! The write side of tag handling: creating, signing, deleting, and pushing
//! tags.
//!
//! Every entry point is idempotent by design — a release that failed after
//! tagging but before pushing must converge on re-run rather than bail, so
//! creating a tag that already points at `HEAD` reuses it (re-creating it
//! signed when an unsigned leftover would otherwise ship) and deleting an
//! already-absent tag succeeds. Reads that inform those decisions live in
//! [`super::position`].

use anyhow::Result;
use std::path::Path;
use std::process::Command;

use super::git_output_in;
use super::position::tag_points_at_head_in;

/// The `git tag` create flag selecting a signed (`-s`) versus unsigned (`-a`)
/// annotated tag. Pure so the flag choice is unit-testable without a real
/// signing key or a git repository.
pub(crate) fn tag_create_flag(sign: bool) -> &'static str {
    if sign { "-s" } else { "-a" }
}

/// Whether the annotated tag object named `tag` embeds a signature block.
///
/// Reads the raw object with `git cat-file tag <tag>` and looks for a GPG or
/// SSH signature armor header. Both signing formats are supported by the tag
/// create path (`gpg.format = ssh` vs the default openpgp), so both headers
/// count — distinguishing a signed tag from unsigned debris a prior run may
/// have left behind.
pub(crate) fn tag_is_signed(cwd: &Path, tag: &str) -> Result<bool> {
    let body = git_output_in(cwd, &["cat-file", "tag", tag])?;
    Ok(body.contains("-----BEGIN PGP SIGNATURE-----")
        || body.contains("-----BEGIN SSH SIGNATURE-----"))
}

/// Create an annotated tag and push it if an `origin` remote exists.
///
/// When `sign` is true the tag is created with `git tag -s` (signed) instead
/// of `git tag -a`; the signing key/method come from the user's git config.
pub fn create_and_push_tag(
    tag: &str,
    message: &str,
    dry_run: bool,
    sign: bool,
    log: &crate::log::StageLogger,
    strict: bool,
) -> Result<()> {
    create_and_push_tag_in(
        &std::env::current_dir()?,
        tag,
        message,
        dry_run,
        sign,
        log,
        strict,
    )
}

/// Create an annotated tag in `cwd` and push it if an `origin` remote exists.
///
/// Path-taking sibling of [`create_and_push_tag`] so callers (notably the
/// GitHub-API tag fallback path and tests) can drive tagging against an
/// explicit repository without mutating the process cwd.
pub fn create_and_push_tag_in(
    cwd: &Path,
    tag: &str,
    message: &str,
    dry_run: bool,
    sign: bool,
    log: &crate::log::StageLogger,
    strict: bool,
) -> Result<()> {
    if dry_run {
        log.status(&format!(
            "(dry-run) would create {}tag {} (\"{}\")",
            if sign { "signed " } else { "" },
            tag,
            message
        ));
        return Ok(());
    }
    git_output_in(cwd, &["tag", tag_create_flag(sign), tag, "-m", message])?;

    if super::has_remote_in(cwd, "origin") {
        git_output_in(cwd, &["push", "origin", tag])?;
    } else if strict {
        anyhow::bail!("no 'origin' remote found, cannot push tag (strict mode)");
    } else {
        log.warn("skipped push — no 'origin' remote found");
    }
    Ok(())
}

/// Create an annotated tag locally without pushing.
///
/// Writes `git tag -a <tag> -m <message>` in `cwd` (or `git tag -s …` when
/// `sign` is true, taking the signing key/method from the user's git config).
/// Does NOT push. The caller is responsible for pushing all tags (typically
/// atomically via [`push_branch_and_tags_atomic_in`]).
pub fn create_tag_local_only(
    cwd: &Path,
    tag: &str,
    message: &str,
    dry_run: bool,
    sign: bool,
    log: &crate::log::StageLogger,
) -> Result<()> {
    if dry_run {
        log.status(&format!(
            "(dry-run) would create local {}tag {} (\"{}\")",
            if sign { "signed " } else { "" },
            tag,
            message
        ));
        return Ok(());
    }
    if let Err(e) = git_output_in(cwd, &["tag", tag_create_flag(sign), tag, "-m", message]) {
        // A prior `tag` run that committed writeback and created the tag but
        // failed to push leaves this exact debris behind; a re-run must be
        // idempotent when the leftover tag already points at the commit we
        // would tag, and actionable (not raw git noise) when it does not.
        let tag_ref = format!("refs/tags/{}", tag);
        if git_output_in(cwd, &["rev-parse", "--verify", "--quiet", &tag_ref]).is_ok() {
            if tag_points_at_head_in(cwd, tag)? {
                // A signed re-run over an UNSIGNED leftover must not silently
                // ship the unsigned tag. A local-only tag is a two-way door, so
                // delete and re-create it signed rather than reuse it.
                if sign && !tag_is_signed(cwd, tag)? {
                    delete_local_tag_in(cwd, tag)?;
                    git_output_in(cwd, &["tag", tag_create_flag(sign), tag, "-m", message])?;
                    log.status(&format!(
                        "tag {} existed unsigned at HEAD; re-created it signed",
                        tag
                    ));
                    return Ok(());
                }
                log.status(&format!(
                    "tag {} already exists and points at HEAD; reusing it",
                    tag
                ));
                return Ok(());
            }
            anyhow::bail!(
                "tag {} already exists but points at a different commit than HEAD \
                 (likely left behind by a previous run); run `anodizer tag rollback` \
                 or delete the stale tag (`git tag -d {}`) and re-run",
                tag,
                tag
            );
        }
        return Err(e);
    }
    Ok(())
}

/// Delete a local tag (`git tag -d <tag>`). Returns `Ok(())` even when the
/// tag is missing so callers can run the delete idempotently.
///
/// `LC_ALL=C` is pinned on the spawn so the "tag not found" substring
/// match is locale-stable; a non-C locale would translate the message
/// and the idempotency check would silently degrade to bail-on-rerun.
pub fn delete_local_tag_in(cwd: &Path, tag: &str) -> Result<()> {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["tag", "-d", tag])
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| anyhow::anyhow!("failed to invoke git tag -d {tag}: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // "tag not found" is fine — caller wanted it gone.
        if stderr.contains("not found") {
            return Ok(());
        }
        anyhow::bail!("git tag -d {tag} failed: {}", stderr.trim());
    }
    Ok(())
}

/// Delete a tag on the `origin` remote (`git push origin :refs/tags/<tag>`).
///
/// Idempotent: when the remote tag is already absent, git exits non-zero
/// with `"remote ref does not exist"` on stderr — that case is treated as
/// success so a rollback re-run after a partially-completed previous pass
/// doesn't surface alarming WARN noise. Any other non-zero exit bubbles
/// up so callers (notably `tag rollback`) can warn-and-continue per tag
/// without aborting the whole pass.
///
/// `LC_ALL=C` is pinned on the spawn so the substring match is
/// locale-stable.
pub fn delete_remote_tag_in(cwd: &Path, tag: &str) -> Result<()> {
    let refspec = format!(":refs/tags/{}", tag);
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["push", "origin", &refspec])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| anyhow::anyhow!("failed to invoke git push origin {refspec}: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Already-absent on the remote → treat as success. Covers both
        // `"remote ref does not exist"` (modern git) and the older
        // `"unable to delete '<refspec>': remote ref does not exist"`
        // wording — substring match catches both.
        if stderr.contains("remote ref does not exist") {
            tracing::warn!(
                "remote tag {tag} already absent on origin — treating as deleted (idempotent)"
            );
            return Ok(());
        }
        let raw = format!("git push origin {} failed: {}", refspec, stderr.trim());
        anyhow::bail!("{}", crate::redact::redact_process_env(&raw));
    }
    Ok(())
}

/// Inputs to [`push_branch_and_tags_atomic_in`].
///
/// Groups the push target (`remote` + optional `branch`), the `tags` to push,
/// and the `dry_run` / `strict` toggles so the public helper reads cleanly at
/// call sites instead of carrying a long positional argument list.
///
/// Ref combinations the helper accepts:
/// - `branch = Some` + non-empty `tags` → `git push --atomic <remote> HEAD:refs/heads/<branch> <tags…>`
/// - `branch = Some` + empty `tags` → `git push <remote> HEAD:refs/heads/<branch>`
/// - `branch = None` + non-empty `tags` → `git push --atomic <remote> <tags…>`
/// - `branch = None` + empty `tags` → no-op (logs a warning)
#[derive(Debug, Clone)]
pub struct AtomicPushSpec<'a> {
    /// Remote name to push to (e.g. `"origin"`).
    pub remote: &'a str,
    /// Branch to push HEAD to as `refs/heads/<branch>`, or `None` to push tags only.
    pub branch: Option<&'a str>,
    /// Tags to push.
    pub tags: &'a [String],
    /// When true, log the would-run push instead of executing it.
    pub dry_run: bool,
    /// When true, a missing remote is an error rather than a skipped no-op.
    pub strict: bool,
}

/// Push an optional `branch` and all `tags` to a `remote` atomically.
///
/// See [`AtomicPushSpec`] for the accepted ref combinations.
///
/// When `spec.dry_run` is true, logs what would happen without executing. When
/// the remote does not exist and `spec.strict` is true, returns an error;
/// otherwise logs a warning and returns `Ok(())`.
///
/// HEAD is pushed to `refs/heads/<branch>` (rather than `<branch>` alone) so
/// detached-HEAD checkouts (notably `actions/checkout@v4` with `ref: <sha>`)
/// work without a local branch ref.
///
/// A non-fast-forward rejection — the most likely failure when pushing a
/// version-sync bump commit — is rewrapped with an actionable message before
/// the raw (redacted) git output.
pub fn push_branch_and_tags_atomic_in(
    cwd: &Path,
    spec: &AtomicPushSpec<'_>,
    log: &crate::log::StageLogger,
) -> Result<()> {
    let AtomicPushSpec {
        remote,
        branch,
        tags,
        dry_run,
        strict,
    } = *spec;

    if dry_run {
        let tag_list = tags.join(", ");
        match branch {
            Some(b) => log.status(&format!(
                "(dry-run) would push branch '{}' + tags [{}] to '{}' atomically",
                b, tag_list, remote
            )),
            None => log.status(&format!(
                "(dry-run) would push tags [{}] to '{}' atomically",
                tag_list, remote
            )),
        }
        return Ok(());
    }

    if branch.is_none() && tags.is_empty() {
        log.warn("nothing to push (no branch, no tags)");
        return Ok(());
    }

    if !super::has_remote_in(cwd, remote) {
        if strict {
            anyhow::bail!("no '{remote}' remote found, cannot push (strict mode)");
        }
        log.warn(&format!("skipped push — no '{remote}' remote found"));
        return Ok(());
    }

    // Nothing to push atomically when the tags list is empty — fall back to a
    // plain branch push. --atomic with a single ref is valid git syntax but
    // misleading in log output and unnecessary for atomicity guarantees.
    if tags.is_empty() {
        let Some(b) = branch else {
            // branch=None + tags empty is rejected by the guard above.
            unreachable!("branch is Some whenever tags is empty (guarded above)")
        };
        log.verbose(&format!(
            "no tags to push; pushing branch '{}' to '{}' without --atomic",
            b, remote
        ));
        let head_refspec = format!("HEAD:refs/heads/{}", b);
        return push_with_ff_hint(cwd, &["push", remote, &head_refspec], remote, branch);
    }

    let head_refspec = branch.map(|b| format!("HEAD:refs/heads/{}", b));
    let mut args: Vec<&str> = vec!["push", "--atomic", remote];
    if let Some(ref rs) = head_refspec {
        args.push(rs.as_str());
    }
    for tag in tags {
        args.push(tag.as_str());
    }
    push_with_ff_hint(cwd, &args, remote, branch)
}

/// Run a `git push …` invocation and, on a non-fast-forward rejection, prepend
/// an actionable hint before the raw (already-redacted) git error.
///
/// `branch` names the release branch in the hint when known; falls back to a
/// generic ref message when pushing tags only.
fn push_with_ff_hint(cwd: &Path, args: &[&str], remote: &str, branch: Option<&str>) -> Result<()> {
    match git_output_in(cwd, args) {
        Ok(_) => Ok(()),
        Err(e) => {
            let raw = e.to_string();
            // `! [rejected]` / `non-fast-forward` are git's stable English
            // markers for a stale-ref rejection (`LC_ALL=C` is pinned on the
            // spawn, so the wording does not localize).
            if raw.contains("[rejected]") || raw.contains("non-fast-forward") {
                let target = match branch {
                    Some(b) => format!("{remote}/{b}"),
                    None => format!("a tag ref on '{remote}'"),
                };
                anyhow::bail!(
                    "push rejected (non-fast-forward): {target} moved since checkout. \
                     Pull/rebase the release branch and re-run, or drop --push to push \
                     the tag only.\n{raw}"
                );
            }
            Err(e)
        }
    }
}
