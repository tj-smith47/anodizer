use super::types::RollbackOpts;
use anodizer_core::git;
use anodizer_core::log::StageLogger;
use anyhow::{Result, bail};

/// Per-tag delete pass: warn-and-continue per tag so a single
/// remote-delete glitch doesn't abandon the surrounding mutation.
/// `dry_run` short-circuits to a status line per tag; `no_push`
/// skips the remote leg.
///
/// The remote leg also deletes the GitHub release AT each tag in
/// `attributed` — the tags a run summary (or `--force`) ties to the attempt
/// being rolled back. A release the attempt owns is reversible state of the
/// aborted attempt; leaving it behind orphans it AND poisons future
/// unsummarized rollbacks, whose burn-evidence probe would read the orphan as
/// proof a prior release shipped. A tag NOT in `attributed` keeps any release
/// it carries (it may be a human's draft or a prior reversible release) — that
/// state is never destroyed. When an owned release cannot be confirmed gone,
/// the tag is KEPT (both remote and local) so the rollback stays retryable
/// rather than orphaning the release under a deleted tag.
pub(super) fn delete_tags(
    cwd: &std::path::Path,
    gh_binary: &std::path::Path,
    deletable: &[String],
    attributed: &std::collections::HashSet<String>,
    opts: &RollbackOpts,
    log: &StageLogger,
) {
    for tag in deletable {
        if opts.dry_run {
            if !opts.no_push {
                if attributed.contains(tag) {
                    log.status(&format!(
                        "(dry-run) would delete the GitHub release at {tag} (if one exists)"
                    ));
                } else {
                    log.status(&format!(
                        "(dry-run) would keep any GitHub release at {tag} \
                         (not attributed to this rollback)"
                    ));
                }
            }
            log.status(&format!("(dry-run) would delete tag {tag} (remote+local)"));
            continue;
        }
        if !opts.no_push {
            match delete_release_at_tag(cwd, gh_binary, tag, attributed.contains(tag), log) {
                ReleaseCleanup::Cleared => {}
                ReleaseCleanup::Retained => {
                    log.warn(&format!(
                        "keeping tag {tag} — its GitHub release could not be removed; \
                         deleting the tag now would orphan the release under a missing tag. \
                         The rollback stays retryable: re-run once the release is gone."
                    ));
                    continue;
                }
            }
            match git::delete_remote_tag_in(cwd, tag) {
                Ok(()) => log.status(&format!("deleted remote tag {tag}")),
                Err(e) => log.warn(&format!(
                    "remote tag delete failed for {tag}: {e} (continuing)"
                )),
            }
        } else {
            log.status(&format!("skipped remote delete for {tag} — --no-push"));
        }
        match git::delete_local_tag_in(cwd, tag) {
            Ok(()) => log.status(&format!("deleted local tag {tag}")),
            Err(e) => log.warn(&format!(
                "local tag delete failed for {tag}: {e} (continuing)"
            )),
        }
    }
}

/// Whether the tag delete may proceed after the release-cleanup attempt.
pub(super) enum ReleaseCleanup {
    /// No release remained that this rollback owns — none existed, it was an
    /// unattributed release deliberately left in place, or an owned one was
    /// deleted. Safe to drop the tag.
    Cleared,
    /// An owned release may still exist (its delete failed, or the lookup was
    /// inconclusive). Keep the tag so the rollback stays retryable rather than
    /// orphaning the release under a deleted tag.
    Retained,
}

/// Clean up the GitHub release at `tag` for a rollback.
///
/// `attributed` is true when a run summary ties this tag to the attempt being
/// rolled back (or `--force` overrode the guard): only then is the release
/// deleted, because only then does anodize know the release belongs to the
/// aborted attempt. For an UNATTRIBUTED tag any release is left in place — it
/// may be a human's draft notes or a prior reversible release, and rollback
/// must never destroy state it cannot attribute.
///
/// Warn-and-continue on every failure. Silently inapplicable (verbose-only
/// note) when origin is not a github.com remote. Returns
/// [`ReleaseCleanup::Retained`] when an owned release could not be confirmed
/// gone, so the caller keeps the tag instead of orphaning the release.
pub(super) fn delete_release_at_tag(
    cwd: &std::path::Path,
    gh_binary: &std::path::Path,
    tag: &str,
    attributed: bool,
    log: &StageLogger,
) -> ReleaseCleanup {
    let (owner, repo) = match git::resolve_github_slug_in(None, None, cwd) {
        Ok(slug) => (slug.owner().to_string(), slug.name().to_string()),
        Err(_) => {
            log.verbose(&format!(
                "skipped GitHub release cleanup for {tag} — origin is not a github.com remote"
            ));
            return ReleaseCleanup::Cleared;
        }
    };
    let endpoint = format!("/repos/{owner}/{repo}/releases/tags/{tag}");
    let release_id = match git::gh_api_get_with_binary(gh_binary, &endpoint, None) {
        Ok(v) => v.get("id").and_then(serde_json::Value::as_u64),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("HTTP 404") || msg.contains("Not Found") {
                log.verbose(&format!(
                    "no GitHub release exists at {tag} — nothing to clean up"
                ));
                return ReleaseCleanup::Cleared;
            }
            // A non-404 lookup failure is inconclusive: an owned release might
            // still exist. Keep the tag for an attributed rollback so we never
            // orphan it; an unattributed tag's release was never ours to delete.
            log.warn(&format!(
                "could not look up the GitHub release at {tag} for cleanup: {msg} (continuing)"
            ));
            return if attributed {
                ReleaseCleanup::Retained
            } else {
                ReleaseCleanup::Cleared
            };
        }
    };
    let Some(id) = release_id else {
        if attributed {
            log.warn(&format!(
                "GitHub release lookup for {tag} returned no numeric id — keeping the tag so \
                 the rollback stays retryable (delete the release manually at \
                 https://github.com/{owner}/{repo}/releases/tag/{tag} if one exists)"
            ));
            return ReleaseCleanup::Retained;
        }
        return ReleaseCleanup::Cleared;
    };
    if !attributed {
        // A release exists but no run evidence attributes it to the attempt
        // being rolled back — never destroy unattributed state. Flag it and let
        // the tag delete proceed; the release simply becomes untagged.
        log.warn(&format!(
            "a GitHub release exists at {tag} but no run summary attributes it to this \
             rollback — leaving it in place. Delete it manually if intended: \
             https://github.com/{owner}/{repo}/releases/tag/{tag}"
        ));
        return ReleaseCleanup::Cleared;
    }
    let delete_endpoint = format!("/repos/{owner}/{repo}/releases/{id}");
    match git::gh_api_delete_with_binary(gh_binary, &delete_endpoint, None) {
        Ok(()) => {
            log.status(&format!(
                "deleted the GitHub release at {tag} (it belonged to the rolled-back attempt)"
            ));
            ReleaseCleanup::Cleared
        }
        Err(e) => {
            log.warn(&format!(
                "GitHub release delete failed for {tag}: {e:#} (keeping the tag so the \
                 rollback stays retryable — re-run once the release is gone, or delete it \
                 manually at https://github.com/{owner}/{repo}/releases/tag/{tag})"
            ));
            ReleaseCleanup::Retained
        }
    }
}

/// Resolve the branch to push the revert commit to.
///
/// Resolution order:
/// 1. `--branch` flag wins unconditionally.
/// 2. SHA-derivation: `git branch -r --contains <bump_sha>`. The bump
///    SHA is the deterministic anchor of the just-rolled-back tag,
///    so it's race-immune to the default branch moving between bump
///    and rollback. Exactly one remote branch → use it. Multiple →
///    require `--branch` to disambiguate.
/// 3. Fallback to [`git::get_current_branch_in`] for repos with no
///    remote (local-only rollback workflows).
pub(super) fn resolve_push_branch(
    cwd: &std::path::Path,
    bump_sha: &str,
    explicit: Option<&str>,
) -> Result<String> {
    resolve_push_branch_with_env(cwd, bump_sha, explicit, &anodizer_core::ProcessEnvSource)
}

/// [`resolve_push_branch`] with the env source injected so the
/// detached-HEAD `GITHUB_REF_NAME` fallback can be driven from a
/// [`MapEnvSource`](anodizer_core::MapEnvSource) in tests rather than
/// mutating the real process environment.
pub(super) fn resolve_push_branch_with_env<E: anodizer_core::EnvSource + ?Sized>(
    cwd: &std::path::Path,
    bump_sha: &str,
    explicit: Option<&str>,
    env: &E,
) -> Result<String> {
    if let Some(b) = explicit {
        return Ok(b.to_string());
    }
    if let Ok(branches) = git::branches_containing_sha_in(cwd, bump_sha) {
        // Drive off the slice directly so the single-branch case needs no
        // `.expect()` on a re-derived `next()`: `[only]` binds the one branch
        // by value, `[_, ..]` (2+) is the ambiguous case, `[]` falls through
        // to the HEAD-resolution fallback below.
        match branches.as_slice() {
            [only] => return Ok(only.clone()),
            [_, ..] => bail!(
                "bump commit {} is reachable from {} remote branches: {}.\n\
                 pass --branch <name> to disambiguate.",
                &bump_sha[..bump_sha.len().min(12)],
                branches.len(),
                branches.join(", ")
            ),
            [] => {}
        }
    }
    match git::get_current_branch_in_with_env(cwd, env) {
        Ok(b) => Ok(b),
        Err(_) => bail!(
            "cannot determine branch for revert push — bump commit {} is \
             not reachable from any remote branch and HEAD resolution failed.\n\
             pass --branch <name> explicitly.",
            &bump_sha[..bump_sha.len().min(12)]
        ),
    }
}
