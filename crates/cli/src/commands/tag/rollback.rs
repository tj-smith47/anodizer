//! `anodize tag rollback` — delete anodize-managed tags at a SHA and
//! revert (or reset to) the bump commit they point at.
//!
//! Failure-recovery counterpart to `anodize tag`: when a downstream
//! `anodize release` poisons a tag (publish failure, mcp 422, etc.) the
//! operator is left with a tag pointing at a bumped-but-broken commit.
//! This subcommand deletes the tag locally + on origin, then either
//! `git revert`s the bump commit (default, history-preserving) or
//! `git reset --hard`s past it (opt-in, history-rewriting).
//!
//! Safety rails:
//! - Tag name regex filter — only anodize-shaped tags are touched
//!   (`vX.Y.Z[-pre][+build]` for lockstep, `<crate>-vX.Y.Z[...]` for
//!   per-crate). Non-matching tags are skipped with a reason printed.
//! - Hard-fail when non-anodize commits sit between the target SHA and
//!   HEAD in `--mode=revert` (protects against rolling back a bump
//!   after unrelated work landed on top). Use `--mode=reset` to force.

use anodizer_core::git;
use anodizer_core::log::{StageLogger, Verbosity};
use anyhow::{Result, bail};
use regex::Regex;
use std::sync::LazyLock;

/// A published-state guard refusal: the rollback was declined BY DESIGN
/// because destroying the tag(s) could only orphan live published state
/// (a one-way-door registry already holds the version). Distinct from a
/// mechanical rollback failure (git error, unreachable network probe,
/// unmappable config): a refusal is final protection with a known next
/// step, not breakage. Callers that drive rollback programmatically
/// (the release failure policy) downcast to this type to render the
/// refusal as protective status output instead of a failure warning.
#[derive(Debug)]
pub struct RollbackRefusal {
    /// Why the rollback was refused — the burn evidence, one line per
    /// affected tag/version.
    pub reason: String,
    /// What the operator should do instead (fix forward / `--force`).
    pub next_step: String,
}

impl std::fmt::Display for RollbackRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to roll back — {}\nnext step: {}",
            self.reason, self.next_step
        )
    }
}

impl std::error::Error for RollbackRefusal {}

/// Canonical fix-forward guidance shared by every refusal site: the
/// version is burned, so the only clean path is the NEXT version;
/// `--force` remains the explicit override.
fn refusal_next_step() -> String {
    "fix the failure and cut the NEXT version (auto-tag mints it from the next push). \
     To override anyway: `anodizer tag rollback --force`."
        .to_string()
}

/// Scope filter for which tag shape(s) to operate on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Both lockstep (`vX.Y.Z`) and per-crate (`<crate>-vX.Y.Z`) tags.
    All,
    /// Only lockstep tags (`vX.Y.Z`).
    Lockstep,
    /// Only per-crate tags (`<crate>-vX.Y.Z`).
    PerCrate,
}

impl std::str::FromStr for Scope {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "all" => Ok(Scope::All),
            "lockstep" => Ok(Scope::Lockstep),
            "per-crate" | "percrate" => Ok(Scope::PerCrate),
            other => Err(format!(
                "invalid --scope value: {other:?} (expected all | lockstep | per-crate)"
            )),
        }
    }
}

/// Rollback strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `git revert --no-edit <sha>` — preserves history. Default.
    Revert,
    /// `git reset --hard <sha>~1` — rewrites history; requires
    /// `--force-with-lease` to push. Opt-in only.
    Reset,
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "revert" => Ok(Mode::Revert),
            "reset" => Ok(Mode::Reset),
            other => Err(format!(
                "invalid --mode value: {other:?} (expected revert | reset)"
            )),
        }
    }
}

pub struct RollbackOpts {
    /// Target SHA. `None` resolves to `HEAD`.
    pub sha: Option<String>,
    pub dry_run: bool,
    pub no_push: bool,
    /// `--force`: override the published-state guard. Without it,
    /// rollback refuses when the tag's run summary shows a one-way-door
    /// (Submitter) publisher landed — the version is burned at a
    /// registry that never accepts the same version twice — when the
    /// crates.io index shows the tag's crate@version live (GLOBAL state:
    /// a prior run may have published it; an unreachable index fails
    /// closed) — or, when no summary exists, when the tag's GitHub
    /// release is published (non-draft).
    pub force: bool,
    pub scope: Scope,
    pub mode: Mode,
    /// Branch to push the revert commit to. `None` triggers
    /// auto-resolution via [`git::get_current_branch_in`]; a hard
    /// failure surfaces when HEAD is detached and no local branch
    /// points at it (the operator must pass `--branch` explicitly).
    pub branch: Option<String>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
}

/// Strict semver-ish per-crate tag pattern: `<crate>-v<MAJOR>.<MINOR>.<PATCH>[-pre][+build]`.
/// The crate-name portion accepts ASCII letters, `_` and `-` as the
/// first char (cargo crate names must start with a letter — digits are
/// rejected), then letters/digits/`_`/`-` for the remainder; the
/// suffix is then asserted to be anodize's `v<semver>` form so a tag like
/// `foo-bar` (no `-v` suffix) doesn't accidentally match.
///
/// Compiled once at first use (the pattern is a compile-time literal) so
/// the classifier doesn't recompile it per tag — same caching idea as
/// `is_branchlike` in `core/git/commits.rs`.
///
/// Drift-risk pair with `core::git::is_branchlike`: that predicate matches
/// the same two anodize tag shapes but with deliberately looser, prefix-only
/// regexes (it answers "is this NOT a tag?" for branch fallback, so it must
/// not over-strict). These rollback patterns are fully anchored and strict
/// on purpose. Keep the two shape definitions in sync when the tag grammar
/// changes — they are intentionally separate, not accidentally duplicated.
static PER_CRATE_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_-]*-v\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?(?:\+[A-Za-z0-9.-]+)?$")
        .expect("static regex compiles")
});

/// Lockstep tag pattern: `v<MAJOR>.<MINOR>.<PATCH>[-pre][+build]`. Compiled
/// once at first use (see [`PER_CRATE_TAG_RE`]).
static LOCKSTEP_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^v\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?(?:\+[A-Za-z0-9.-]+)?$")
        .expect("static regex compiles")
});

/// Classification used to filter tags against the requested `--scope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagKind {
    Lockstep,
    PerCrate,
}

/// Classify a tag against anodize's naming conventions. Returns `None`
/// when the tag doesn't match either shape (in which case the rollback
/// command leaves it alone).
fn classify_tag(tag: &str) -> Option<TagKind> {
    // Lockstep first — `vX.Y.Z` would also fail the per-crate regex's
    // `<crate>-` prefix requirement, but the explicit ordering keeps the
    // intent obvious to a reader.
    if LOCKSTEP_TAG_RE.is_match(tag) {
        Some(TagKind::Lockstep)
    } else if PER_CRATE_TAG_RE.is_match(tag) {
        Some(TagKind::PerCrate)
    } else {
        None
    }
}

/// Apply the `--scope` filter on top of the classification.
fn scope_includes(scope: Scope, kind: TagKind) -> bool {
    matches!(
        (scope, kind),
        (Scope::All, _)
            | (Scope::Lockstep, TagKind::Lockstep)
            | (Scope::PerCrate, TagKind::PerCrate)
    )
}

/// Build the rollback commit subject line. The tags list goes in the
/// body so a long per-crate batch doesn't blow past 72 chars. When
/// `dry_run` is true, the tag list is prefixed with "WOULD be" to
/// signal that the preview commit message describes pending (not
/// actually applied) state — otherwise a `--dry-run` printout reads
/// identically to a real-run one and fools the operator.
fn build_revert_message(target_sha: &str, deleted_tags: &[String], dry_run: bool) -> String {
    let primary = deleted_tags
        .iter()
        .find(|t| LOCKSTEP_TAG_RE.is_match(t))
        .cloned()
        .unwrap_or_else(|| {
            deleted_tags
                .first()
                .cloned()
                .unwrap_or_else(|| "release".to_string())
        });
    let short = if target_sha.len() > 7 {
        &target_sha[..7]
    } else {
        target_sha
    };
    let mut body = format!(
        "{} {primary} [skip ci]\n\nReverts {short}.",
        rollback_subject_prefix()
    );
    if !deleted_tags.is_empty() {
        let label = if dry_run {
            "Tags that WOULD be deleted"
        } else {
            "Tags deleted"
        };
        body.push_str(&format!("\n{label}: {}", deleted_tags.join(", ")));
    }
    body
}

/// Subject prefix of anodize's own rollback commits
/// (`chore(release): rollback …`), composed from the shared
/// release-machinery prefix so the writer ([`build_revert_message`]) and
/// the safety-check matcher below can never drift apart.
fn rollback_subject_prefix() -> String {
    format!("{}rollback", git::RELEASE_COMMIT_PREFIX)
}

/// Prefix that a plain `git revert` of an anodize release-machinery commit
/// produces (the amend-failure window, where the custom rollback subject
/// was never applied). Used by the rollback safety check to recognise its
/// own prior revert commit (so re-runs are idempotent) without absorbing
/// unrelated `Revert "<...>"` commits that GitHub's "Revert this PR"
/// button emits with arbitrary upstream subjects. Composed from the shared
/// prefix the bump/rollback writers stamp.
static ANODIZE_REVERT_SUBJECT_PREFIX: LazyLock<String> =
    LazyLock::new(|| format!("Revert \"{}", git::RELEASE_COMMIT_PREFIX));

pub fn run(opts: RollbackOpts) -> Result<()> {
    run_with_gh(opts, std::path::Path::new("gh"))
}

/// Path-taking sibling of [`run`]: `gh_binary` is the `gh` CLI used by
/// the published-state guard's GitHub-release fallback probe.
/// Production passes `Path::new("gh")` (PATH lookup); tests point at a
/// stub script so no global PATH mutation is needed (same seam
/// convention as `core::git::gh_api_get_with_binary`).
fn run_with_gh(opts: RollbackOpts, gh_binary: &std::path::Path) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let log = StageLogger::new(
        "tag-rollback",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    let raw_target = opts.sha.as_deref().unwrap_or("HEAD");
    let target_sha = git::rev_parse_in(&cwd, raw_target)?;
    log.kv(
        "target",
        &format!("{} ({})", raw_target, short(&target_sha)),
        "target".len(),
    );

    let all_tags_at_sha = git::get_tags_at_sha_in(&cwd, &target_sha)?;
    if all_tags_at_sha.is_empty() {
        log.warn(&format!("no tags found at {}", short(&target_sha)));
        bail!(
            "refusing to roll back: no tags point at {} — pass the bumped commit's SHA explicitly",
            short(&target_sha)
        );
    }

    let mut deletable: Vec<String> = Vec::new();
    for tag in &all_tags_at_sha {
        match classify_tag(tag) {
            None => log.status(&format!("skipped {tag} (not anodize-shaped)")),
            Some(kind) if !scope_includes(opts.scope, kind) => log.status(&format!(
                "skipped {tag} (scope filter --scope={:?})",
                opts.scope
            )),
            Some(_) => deletable.push(tag.clone()),
        }
    }

    if deletable.is_empty() {
        log.warn(&format!(
            "no anodize-managed tags at {} match --scope={:?}",
            short(&target_sha),
            opts.scope
        ));
        return Ok(());
    }

    // Published-state guard, BEFORE any mutation (including dry-run,
    // so the preview reports the same refusal the real run would).
    // A one-way-door (Submitter) publisher that landed for one of these
    // tags burned the version: registries like crates.io / chocolatey /
    // winget / snapcraft never accept the same version twice, so
    // deleting the tag + reverting the bump can never lead to a clean
    // same-version re-cut — only to an orphaned live release.
    // Tags whose GitHub release this rollback owns (a run summary attributes
    // them to the attempt being rolled back, or --force overrode the guard).
    // Only these get their release deleted; an unattributed tag's release is
    // preserved (it may be a human's draft or a prior reversible release).
    let attributed: std::collections::HashSet<String> = if opts.force {
        log.warn("skipped the published-state guard — --force");
        deletable.iter().cloned().collect()
    } else {
        // Fail-closed config load: the config drives the dist-dir resolution
        // for run summaries and the tag→crate mapping for the crates.io index
        // probe. A missing or unparseable config would blind the probe — the
        // exact failure mode the guard exists to prevent — so it refuses
        // instead of silently narrowing the evidence (a network error already
        // refuses; a config error must not be weaker). The probe itself
        // reuses the publish stage's sparse-index client so rollback and
        // publish can never disagree about what "published on crates.io"
        // means.
        let repo_config = match crate::pipeline::load_repo_config(&cwd) {
            Ok(config) => config,
            Err(e) => bail!(
                "refusing to roll back — could not load the anodizer config: {e:#}\n\
                 The published-state guard needs the config to map the tag(s) to crates \
                 for the crates.io burn probe; without that mapping there is no proof the \
                 version(s) are safe to destroy — a prior run may have burned them on a \
                 one-way-door registry. Fix the config, or run from a checkout whose \
                 config parses (e.g. the directory that contains it). As a last resort, \
                 --force skips ALL published-state checks (run summaries, crates.io, \
                 GitHub releases), not just this config probe — use it only if you are \
                 certain nothing irreversible shipped."
            ),
        };
        let retry_policy = repo_config.retry.unwrap_or_default().to_policy();
        let index_probe = |name: &str, version: &str| {
            anodizer_stage_publish::cargo::published_on_crates_io(
                name,
                version,
                &retry_policy,
                &log,
            )
        };
        let unsummarized = check_not_irreversibly_published(
            &cwd,
            gh_binary,
            &deletable,
            &repo_config,
            &index_probe,
            &log,
        )?;
        deletable
            .iter()
            .filter(|t| !unsummarized.contains(t))
            .cloned()
            .collect()
    };

    // Safety check (--mode=revert only). Non-bump commits on top of
    // the target SHA mean someone landed unrelated work since the
    // bump; reverting blindly would lose it. Tolerate only anodize's
    // OWN prior revert commit so re-runs are idempotent — a generic
    // `"Revert "<...>"` prefix would silently absorb GitHub's
    // "Revert this PR" button output (e.g. an unrelated feature
    // revert) and disable the safety net.
    if opts.mode == Mode::Revert {
        let intervening = git::commits_with_subjects_in(&cwd, &target_sha)?;
        let mut suspicious: Vec<(String, String)> = Vec::new();
        for (sha, subject) in &intervening {
            if subject.starts_with(ANODIZE_REVERT_SUBJECT_PREFIX.as_str())
                || subject.starts_with(&rollback_subject_prefix())
            {
                continue;
            }
            suspicious.push((sha.clone(), subject.clone()));
        }
        if !suspicious.is_empty() {
            let mut msg = format!(
                "cannot rollback — {} non-bump commit(s) sit between HEAD and {}:\n",
                suspicious.len(),
                short(&target_sha)
            );
            for (sha, subj) in &suspicious {
                msg.push_str(&format!("  {} {}\n", short(sha), subj));
            }
            msg.push_str("resolve manually, or use --mode=reset to force.");
            bail!("{msg}");
        }
    }

    // Local mutation runs FIRST so a failed revert / reset leaves the
    // remote tags intact. Operator can retry without staring down a
    // half-rolled-back remote (tag gone) + intact local (tag still
    // present + bump commit still HEAD). Per-tag remote delete happens
    // after the local mutation succeeds — if a single remote-delete
    // glitches, the revert is already on disk and ready to push.

    // Mode=reset short-circuits revert+push entirely. Print a loud
    // warning so the operator knows they own the force-push.
    if opts.mode == Mode::Reset {
        let parent = format!("{}~1", target_sha);
        if opts.dry_run {
            log.status(&format!(
                "(dry-run) would run: git reset --hard {} (parent of bump commit)",
                short(&target_sha)
            ));
        } else {
            git::reset_hard_in(&cwd, &parent)?;
            log.status(&format!(
                "reset HEAD to {} (parent of bump commit)",
                short(&target_sha)
            ));
        }
        delete_tags(&cwd, gh_binary, &deletable, &attributed, &opts, &log);
        log.warn(
            "--mode=reset rewrote local history. Push with \
             `git push --force-with-lease origin <branch>` when ready.",
        );
        return Ok(());
    }

    // Mode=revert: create the revert commit, PUSH it, then delete tags.
    // Push precedes the remote tag delete so a push failure (e.g. a
    // non-fast-forward) leaves the tags intact and the rollback safely
    // retryable — never a tag-deleted-but-commit-unpushed limbo. The commit
    // message lists the tags that WILL be deleted (or under --dry-run, that
    // WOULD be deleted).
    let message = build_revert_message(&target_sha, &deletable, opts.dry_run);
    if opts.dry_run {
        log.status(&format!(
            "(dry-run) would run: git revert --no-edit {} && git commit --amend -m {:?}",
            short(&target_sha),
            message
        ));
    } else {
        let identity = git::resolve_rollback_identity(&cwd);
        git::revert_commit_in(&cwd, &target_sha, Some(&message), &identity)?;
        log.status(&format!("created revert commit {}", first_line(&message)));
    }

    if opts.no_push {
        delete_tags(&cwd, gh_binary, &deletable, &attributed, &opts, &log);
        log.status("skipped branch push — --no-push");
        return Ok(());
    }
    let branch = resolve_push_branch(&cwd, &target_sha, opts.branch.as_deref())?;
    if opts.dry_run {
        log.status(&format!("(dry-run) would run: git push origin {branch}"));
        delete_tags(&cwd, gh_binary, &deletable, &attributed, &opts, &log);
    } else {
        // Push BEFORE deleting remote tags: the destructive tag delete is the
        // last step, so a push failure aborts before any tag is dropped.
        git::push_branch_in(&cwd, &branch)?;
        log.status(&format!("pushed revert to origin/{branch}"));
        delete_tags(&cwd, gh_binary, &deletable, &attributed, &opts, &log);
    }
    Ok(())
}

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
fn delete_tags(
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
enum ReleaseCleanup {
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
fn delete_release_at_tag(
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
fn resolve_push_branch(
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
fn resolve_push_branch_with_env<E: anodizer_core::EnvSource + ?Sized>(
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

/// Refuse rollback when the version is already burned at a one-way-door
/// (Submitter group) publisher, by evidence strength:
///
/// 1. Run summaries on disk (`<dist>/run-*/summary.json`, plus
///    `<dist>/<crate>/run-*/summary.json` in per-crate workspaces)
///    whose `tag` matches a tag about to be deleted — the
///    per-publisher truth written by the release run itself, including
///    failed runs. A summary that shows a landed Submitter REFUSES.
/// 2. The crates.io sparse index, for every tag that maps (via the repo
///    config's crate tag families) to a crates.io-targeting crate. The
///    run summary answers a PER-RUN question; whether a version is
///    burned on a one-way-door registry is GLOBAL state — a PRIOR run
///    may have published it, and that run's summary lives on another
///    runner. A version live on the index REFUSES even when this run's
///    summary is clean; an unreachable index FAILS CLOSED (publication
///    state unverifiable). A tag that maps to NO crate while the config
///    publishes to crates.io also fails closed (the mapping is the
///    probe's eyes); a tag whose mapped crates simply don't target
///    crates.io carries no cargo one-way door and proceeds.
/// 3. Only for tags with no matching summary (e.g. a fresh checkout
///    that never ran the release): fall back to probing the GitHub
///    Releases API for a published (non-draft) release at the tag.
///
/// Only a tag that clears every applicable layer is rolled back;
/// reversible-only evidence (github-release assets, blobs,
/// tap/bucket/index commits) permits rollback because their state can
/// be deleted and the same version re-cut.
///
/// `index_probe` is `(crate_name, version) -> published?` — production
/// wires [`anodizer_stage_publish::cargo::published_on_crates_io`];
/// tests inject stubs (same seam convention as `gh_binary`).
///
/// On success returns the subset of `tags` that had NO matching run summary
/// (the "unattributed" tags). The caller uses that to decide release cleanup:
/// a summarized tag's GitHub release belongs to the run being rolled back and
/// may be deleted, while an unattributed tag's release is left untouched.
fn check_not_irreversibly_published(
    cwd: &std::path::Path,
    gh_binary: &std::path::Path,
    tags: &[String],
    repo_config: &anodizer_core::config::Config,
    index_probe: &dyn Fn(&str, &str) -> Result<bool>,
    log: &StageLogger,
) -> Result<Vec<String>> {
    let summaries = collect_run_summaries(&resolve_dist_dir(cwd, repo_config), log);
    let mut burned: Vec<(String, Vec<String>)> = Vec::new();
    let mut unsummarized: Vec<String> = Vec::new();
    for tag in tags {
        let matching: Vec<_> = summaries.iter().filter(|s| s.tag == *tag).collect();
        if matching.is_empty() {
            unsummarized.push(tag.clone());
            continue;
        }
        let mut names: Vec<String> = matching
            .iter()
            .flat_map(|s| s.burned_submitter_names())
            .collect();
        names.sort();
        names.dedup();
        // `irreversibly_published` is the precomputed verdict;
        // `burned_submitter_names` additionally catches summaries
        // written before the flag existed.
        if matching.iter().any(|s| s.irreversibly_published) || !names.is_empty() {
            burned.push((tag.clone(), names));
        } else {
            log.status(&format!(
                "no one-way-door publisher landed for {tag} per this run's summary"
            ));
        }
    }
    if !burned.is_empty() {
        let detail = burned
            .iter()
            .map(|(tag, names)| {
                if names.is_empty() {
                    format!("  {tag}: run summary records an irreversible publish")
                } else {
                    format!("  {tag}: version burned at {}", names.join(", "))
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Err(RollbackRefusal {
            reason: format!(
                "one-way-door publisher(s) already accepted these version(s):\n\
                 {detail}\n\
                 Those registries never accept the same version twice, so deleting the \
                 tag(s) and reverting the bump cannot lead to a clean same-version re-cut \
                 — tags kept to protect the published state."
            ),
            next_step: refusal_next_step(),
        }
        .into());
    }
    check_not_burned_on_crates_io(tags, &unsummarized, repo_config, index_probe, log)?;
    if unsummarized.is_empty() {
        return Ok(unsummarized);
    }
    check_no_published_releases(cwd, gh_binary, &unsummarized, log)?;
    Ok(unsummarized)
}

/// Dist-dir resolution for the published-state guard: the repo config's
/// `dist:`. Relative values anchor at `cwd`.
fn resolve_dist_dir(
    cwd: &std::path::Path,
    repo_config: &anodizer_core::config::Config,
) -> std::path::PathBuf {
    let dist = repo_config.dist.clone();
    if dist.is_absolute() {
        dist
    } else {
        cwd.join(dist)
    }
}

/// How a tag maps onto the config's crate universe for the crates.io burn
/// probe. The split lets the guard distinguish "nothing to probe because
/// none of the tag's crates target crates.io" (safe to proceed) from
/// "the tag maps to no crate at all" (the probe is blind — fail closed
/// when the config publishes to crates.io elsewhere).
struct TagCrateMapping {
    /// `(crate name, version)` pairs the tag stamps on crates.io.
    probes: Vec<(String, String)>,
    /// Crates whose tag family matched but which don't publish to
    /// crates.io (no `publish.cargo` block, or a custom `registry:`/
    /// `index:` target outside the probe's scope).
    matched_non_crates_io: usize,
}

/// Resolve the `(crate name, version)` pairs a tag stamps on crates.io, per
/// the repo config: every crate whose `publish.cargo` block targets
/// crates.io (per the publisher's own [`targets_crates_io`] judgment —
/// custom `registry:`/`index:` targets are out of the probe's scope) and
/// whose tag family prefix (from its `tag_template`, monorepo prefix
/// stripped) matches the tag. Per-crate tags (`crd-v0.5.0`) resolve to
/// their own crate — note the tag prefix is the template's, NOT the crate
/// name (cfgd's `crd-v...` family belongs to the crate `cfgd-crd`);
/// lockstep tags (every crate sharing the bare `v...` family) resolve to
/// every such crate.
///
/// Publish-time `skip:`/`if:` gating is deliberately NOT evaluated (no
/// template context exists in a rollback): a gated crate may be probed even
/// though the release never publishes it, which can only tighten the guard
/// (`--force` remains the escape hatch), never loosen it.
///
/// [`targets_crates_io`]: anodizer_stage_publish::cargo::targets_crates_io
fn crates_io_versions_for_tag(
    config: &anodizer_core::config::Config,
    tag: &str,
) -> TagCrateMapping {
    let stripped = match config.monorepo_tag_prefix() {
        Some(prefix) => git::strip_monorepo_prefix(tag, prefix),
        None => tag,
    };
    let mut mapping = TagCrateMapping {
        probes: Vec::new(),
        matched_non_crates_io: 0,
    };
    for c in config.crate_universe() {
        let prefix = git::per_crate_tag_prefix(&c.name, &c.tag_template);
        let Some(version) = stripped.strip_prefix(&prefix) else {
            continue;
        };
        if git::parse_semver(version).is_err() {
            continue;
        }
        match c.publish.as_ref().and_then(|p| p.cargo.as_ref()) {
            Some(cargo_cfg)
                if anodizer_stage_publish::cargo::targets_crates_io(Some(cargo_cfg)) =>
            {
                mapping.probes.push((c.name.clone(), version.to_string()));
            }
            _ => mapping.matched_non_crates_io += 1,
        }
    }
    mapping
}

/// Layer 2 of [`check_not_irreversibly_published`]: refuse rollback when
/// any tag's crates.io-targeting crate@version is live on the crates.io
/// sparse index — GLOBAL registry state, consulted regardless of what this
/// run's summaries say (a prior run may have burned the version; its
/// summary lives on another runner's disk).
///
/// - version on the index → REFUSE (burned; fix forward).
/// - index unreachable → REFUSE (fail closed: publication state is
///   unverifiable, and gambling a destructive delete on a transient outage
///   is the poison-guard anti-pattern). `--force` is the operator escape.
/// - tag maps to NO crate while the config publishes to crates.io →
///   REFUSE (fail closed: the tag→crate mapping is the probe's eyes; a
///   tag it cannot map might version a crate that IS burned).
/// - tag maps only to crates that don't target crates.io, or the config
///   publishes nothing to crates.io at all → proceed: there is no cargo
///   one-way door for this config to have burned.
///
/// Repeated `crate@version` probes are deduplicated (the same pair recurs
/// under `Scope::All` when tag families overlap, e.g. a monorepo-prefixed
/// and a bare tag resolving to the same crate).
fn check_not_burned_on_crates_io(
    tags: &[String],
    unsummarized: &[String],
    config: &anodizer_core::config::Config,
    index_probe: &dyn Fn(&str, &str) -> Result<bool>,
    log: &StageLogger,
) -> Result<()> {
    let config_targets_crates_io = config.crate_universe().iter().any(|c| {
        c.publish
            .as_ref()
            .and_then(|p| p.cargo.as_ref())
            .is_some_and(|cfg| anodizer_stage_publish::cargo::targets_crates_io(Some(cfg)))
    });
    if !config_targets_crates_io {
        log.status(
            "no crate in the config publishes to crates.io — no cargo one-way door to probe",
        );
        return Ok(());
    }
    let mut burned: Vec<String> = Vec::new();
    let mut squat_suspect_crates: Vec<String> = Vec::new();
    let mut indeterminate: Vec<String> = Vec::new();
    let mut unmapped: Vec<String> = Vec::new();
    let mut probed: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for tag in tags {
        let mapping = crates_io_versions_for_tag(config, tag);
        if mapping.probes.is_empty() {
            if mapping.matched_non_crates_io > 0 {
                log.status(&format!(
                    "no crates.io-targeting crate is versioned by {tag} — no cargo \
                     one-way door to probe"
                ));
            } else {
                unmapped.push(format!("  {tag}"));
            }
            continue;
        }
        for (name, version) in mapping.probes {
            if !probed.insert((name.clone(), version.clone())) {
                continue;
            }
            match index_probe(&name, &version) {
                Ok(true) => {
                    if unsummarized.contains(tag) && !squat_suspect_crates.contains(&name) {
                        squat_suspect_crates.push(name.clone());
                    }
                    burned.push(format!("  {tag}: {name}@{version}"));
                }
                Ok(false) => log.status(&format!(
                    "'{name}@{version}' is not on the crates.io index — {tag} carries no \
                     cargo one-way door"
                )),
                Err(e) => indeterminate.push(format!("  {tag}: {name}@{version} ({e:#})")),
            }
        }
    }
    if !burned.is_empty() {
        // A local run summary is per-runner and ephemeral: a fresh CI runner
        // holds no summary for a burn a prior runner landed, so its absence
        // is expected for a legitimate own-publish and is NOT evidence of
        // foreign ownership. The note leads with that likely case and offers
        // the crates.io page only so the rare squatting possibility can be
        // ruled out — it never implies the version isn't the operator's own.
        let squat_note = if squat_suspect_crates.is_empty() {
            String::new()
        } else {
            let urls = squat_suspect_crates
                .iter()
                .map(|name| format!("https://crates.io/crates/{name}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "\nNo local run summary corroborates this publish — most likely a prior \
                 run of yours (on CI, summaries live on each runner's disk and don't \
                 carry over); far less likely, the name is held by someone else. Confirm \
                 ownership at {urls} before assuming either."
            )
        };
        return Err(RollbackRefusal {
            reason: format!(
                "these version(s) are live on the crates.io index (published by a prior \
                 attempt, whatever this run's summaries say):\n{}\n\
                 crates.io never accepts the same version twice, so deleting the tag(s) \
                 cannot lead to a clean same-version re-cut — tags kept to protect the \
                 published state.{squat_note}",
                burned.join("\n")
            ),
            next_step: refusal_next_step(),
        }
        .into());
    }
    if !indeterminate.is_empty() {
        bail!(
            "refusing to roll back: the crates.io index could not be reached to verify \
             whether these version(s) are already published:\n{}\n\
             Without the index there is no proof the version(s) are safe to destroy — a \
             prior run may have burned them on crates.io. Restore network access and \
             retry, or pass --force if you are certain nothing irreversible shipped.",
            indeterminate.join("\n")
        );
    }
    if !unmapped.is_empty() {
        bail!(
            "refusing to roll back — could not map these tag(s) to any crate in the \
             anodizer config:\n{}\n\
             The crates.io burn probe works by mapping each tag's family (from the crates' \
             tag_template) to the crates it versions, and this config publishes crate(s) to \
             crates.io — a tag the probe cannot map might version a crate whose version is \
             already burned there, so proceeding blind is not safe. Check that the config's \
             crates/tag_template families cover these tag(s), or pass --force if you are \
             certain nothing irreversible shipped.",
            unmapped.join("\n")
        );
    }
    Ok(())
}

/// Collect every parseable run summary under `<dist>/run-*/summary.json`
/// (single-crate / lockstep layout) and `<dist>/<crate>/run-*/summary.json`
/// (per-crate workspace layout). Unreadable or unparseable files warn
/// and are skipped — they carry no usable evidence either way.
fn collect_run_summaries(
    dist: &std::path::Path,
    log: &StageLogger,
) -> Vec<anodizer_stage_publish::run_summary::RunSummary> {
    let mut out = Vec::new();
    for path in anodizer_stage_publish::run_summary::collect_run_summary_paths(dist) {
        match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(serde_json::from_str(&text)?))
        {
            Ok(summary) => out.push(summary),
            Err(e) => log.warn(&format!(
                "ignoring unreadable run summary {}: {e:#}",
                path.display()
            )),
        }
    }
    out
}

/// Outcome of probing GitHub for a release at a tag.
#[derive(Debug)]
enum ReleaseProbe {
    /// A non-draft release exists — rollback must refuse.
    Published,
    /// No release, or only a draft (drafts are reversible).
    NotBlocking,
    /// The probe could not determine release state (gh missing, auth /
    /// network error, ...). The guard FAILS CLOSED on this: with a
    /// GitHub-shaped origin and no run summary, an unanswerable probe
    /// leaves a real possibility that a published release (and burned
    /// one-way-door versions behind it) exists — proceeding would
    /// gamble irreversible state on a transient outage. `--force` is
    /// the operator escape for genuinely-offline recovery.
    Indeterminate(String),
}

/// Probe the GitHub Releases API for a release at `tag`.
///
/// `gh_binary` is the path to the `gh` CLI; production passes
/// `Path::new("gh")` (PATH lookup), tests point at a stub script so no
/// global PATH mutation is needed.
fn probe_release_for_tag(
    gh_binary: &std::path::Path,
    owner: &str,
    repo: &str,
    tag: &str,
) -> ReleaseProbe {
    let endpoint = format!("/repos/{owner}/{repo}/releases/tags/{tag}");
    match git::gh_api_get_with_binary(gh_binary, &endpoint, None) {
        // Missing `draft` counts as published: an API response that
        // omits the field gives no proof the release is reversible.
        Ok(v) => match v.get("draft").and_then(serde_json::Value::as_bool) {
            Some(true) => ReleaseProbe::NotBlocking,
            Some(false) | None => ReleaseProbe::Published,
        },
        Err(e) => {
            let msg = e.to_string();
            // gh surfaces missing releases as `HTTP 404: Not Found`.
            if msg.contains("HTTP 404") || msg.contains("Not Found") {
                ReleaseProbe::NotBlocking
            } else {
                ReleaseProbe::Indeterminate(msg)
            }
        }
    }
}

/// Refuse rollback when any tag about to be deleted carries a
/// published (non-draft) GitHub release.
///
/// Fallback layer of [`check_not_irreversibly_published`], consulted
/// only for tags with no run summary on disk: a published release is
/// the strongest remaining signal that one-way-door publishers shipped
/// alongside it.
///
/// Indeterminate probes (gh CLI missing, auth / network errors other
/// than 404) FAIL CLOSED — refuse with the probe error and point at
/// `--force`: with no summary and no probe answer there is zero
/// evidence the version is safe to destroy. An unresolvable `origin`
/// remote (none configured, or git itself erroring) also fails closed
/// for the same reason. The single fail-OPEN bound: a resolvable
/// origin that is not `github.com`-shaped (GitLab / Gitea / file path /
/// GitHub Enterprise host) warns and proceeds — the probe targets the
/// github.com Releases API, which cannot host a release for such a
/// remote, so it carries no signal either way; run-summary evidence
/// (layer 1 of the guard) remains the only signal for those hosts.
fn check_no_published_releases(
    cwd: &std::path::Path,
    gh_binary: &std::path::Path,
    tags: &[String],
    log: &StageLogger,
) -> Result<()> {
    let (owner, repo) = match git::resolve_github_slug_in(None, None, cwd) {
        Ok(slug) => (slug.owner().to_string(), slug.name().to_string()),
        Err(e) if git::has_remote_in(cwd, "origin") => {
            // The slug resolver already redacts URL credentials in its
            // parse-failure message, so `e` is safe to surface.
            log.warn(&format!(
                "skipped the published-release probe — origin is not a github.com \
                 remote ({e}); no github.com release can exist there \
                 (run-summary evidence still applies)"
            ));
            return Ok(());
        }
        Err(e) => {
            bail!(
                "refusing to roll back: could not resolve the 'origin' remote to run the \
                 published-release guard ({e}).\n\
                 No run summary covers these tag(s) and without a remote there is no \
                 evidence the version(s) are safe to destroy. Configure the 'origin' \
                 remote and retry, or pass --force if you are certain nothing \
                 irreversible shipped.",
            );
        }
    };
    let mut published: Vec<&str> = Vec::new();
    let mut indeterminate: Vec<(&str, String)> = Vec::new();
    for tag in tags {
        match probe_release_for_tag(gh_binary, &owner, &repo, tag) {
            ReleaseProbe::Published => published.push(tag),
            ReleaseProbe::NotBlocking => {}
            ReleaseProbe::Indeterminate(msg) => indeterminate.push((tag, msg)),
        }
    }
    if !indeterminate.is_empty() {
        let detail = indeterminate
            .iter()
            .map(|(tag, msg)| format!("  {tag}: {msg}"))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "refusing to roll back: could not determine whether published GitHub \
             release(s) exist for:\n{detail}\n\
             No run summary covers these tag(s) and the release probe is \
             unanswerable, so there is no evidence the version(s) are safe to \
             destroy. Restore gh / network access (or GITHUB_TOKEN auth) and retry, \
             or pass --force if you are certain nothing irreversible shipped.",
        );
    }
    if !published.is_empty() {
        return Err(RollbackRefusal {
            reason: format!(
                "published GitHub release(s) exist for: {} \
                 (and no run summary is available to prove nothing irreversible shipped).\n\
                 One-way-door publishers (crates.io, chocolatey, winget, snapcraft, ...) \
                 usually ship alongside a published release; if any did, the version is \
                 burned and deleting the tag(s) only orphans live published state — \
                 tags kept to protect it.\n\
                 Caveat: a release left behind by a rollback that predates automatic \
                 release cleanup may be an ORPHAN of a rolled-back attempt rather than \
                 real burn evidence — verify the release (and the one-way-door \
                 registries) before trusting it; if it is an orphan, delete it and \
                 re-run, or use --force.",
                published.join(", ")
            ),
            next_step: refusal_next_step(),
        }
        .into());
    }
    Ok(())
}

/// Trim a SHA to the canonical 7-char short form for log output.
fn short(sha: &str) -> &str {
    if sha.len() > 7 { &sha[..7] } else { sha }
}

/// First line of a multi-line commit message, for compact status lines.
fn first_line(msg: &str) -> &str {
    msg.lines().next().unwrap_or(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_lockstep_release_tags() {
        assert_eq!(classify_tag("v1.2.3"), Some(TagKind::Lockstep));
        assert_eq!(classify_tag("v0.0.1"), Some(TagKind::Lockstep));
        assert_eq!(classify_tag("v10.20.30"), Some(TagKind::Lockstep));
    }

    #[test]
    fn classifies_lockstep_prerelease_and_build_tags() {
        assert_eq!(classify_tag("v1.2.3-rc.1"), Some(TagKind::Lockstep));
        assert_eq!(classify_tag("v1.2.3-beta.10"), Some(TagKind::Lockstep));
        assert_eq!(classify_tag("v1.2.3+build.42"), Some(TagKind::Lockstep));
        assert_eq!(
            classify_tag("v1.2.3-rc.1+build.42"),
            Some(TagKind::Lockstep)
        );
    }

    #[test]
    fn classifies_per_crate_tags() {
        assert_eq!(classify_tag("mycrate-v1.2.3"), Some(TagKind::PerCrate));
        assert_eq!(
            classify_tag("cfgd-operator-v0.4.0"),
            Some(TagKind::PerCrate)
        );
        assert_eq!(
            classify_tag("my_crate-v1.2.3-rc.1"),
            Some(TagKind::PerCrate)
        );
    }

    #[test]
    fn rejects_non_anodize_shaped_tags() {
        assert_eq!(classify_tag("foo-bar"), None);
        assert_eq!(classify_tag("v1.2"), None);
        assert_eq!(classify_tag("v1"), None);
        assert_eq!(classify_tag("release-1.2.3"), None);
        assert_eq!(classify_tag("tag-without-version"), None);
        assert_eq!(classify_tag(""), None);
        assert_eq!(classify_tag("v1.2.3.4"), None);
    }

    #[test]
    fn scope_lockstep_excludes_per_crate() {
        assert!(scope_includes(Scope::Lockstep, TagKind::Lockstep));
        assert!(!scope_includes(Scope::Lockstep, TagKind::PerCrate));
    }

    #[test]
    fn scope_per_crate_excludes_lockstep() {
        assert!(scope_includes(Scope::PerCrate, TagKind::PerCrate));
        assert!(!scope_includes(Scope::PerCrate, TagKind::Lockstep));
    }

    #[test]
    fn scope_all_accepts_both() {
        assert!(scope_includes(Scope::All, TagKind::Lockstep));
        assert!(scope_includes(Scope::All, TagKind::PerCrate));
    }

    #[test]
    fn scope_parser_round_trip() {
        assert_eq!("all".parse::<Scope>().unwrap(), Scope::All);
        assert_eq!("lockstep".parse::<Scope>().unwrap(), Scope::Lockstep);
        assert_eq!("per-crate".parse::<Scope>().unwrap(), Scope::PerCrate);
        assert_eq!("percrate".parse::<Scope>().unwrap(), Scope::PerCrate);
        assert!("nope".parse::<Scope>().is_err());
    }

    #[test]
    fn mode_parser_round_trip() {
        assert_eq!("revert".parse::<Mode>().unwrap(), Mode::Revert);
        assert_eq!("reset".parse::<Mode>().unwrap(), Mode::Reset);
        assert!("rewind".parse::<Mode>().is_err());
    }

    #[test]
    fn revert_message_uses_lockstep_as_subject() {
        let msg = build_revert_message(
            "abcdef1234567890",
            &[
                "mycrate-v1.0.0".into(),
                "v1.0.0".into(),
                "other-v1.0.0".into(),
            ],
            false,
        );
        assert!(msg.starts_with("chore(release): rollback v1.0.0 [skip ci]"));
        assert!(msg.contains("Reverts abcdef1."));
        assert!(msg.contains("Tags deleted: mycrate-v1.0.0, v1.0.0, other-v1.0.0"));
    }

    #[test]
    fn revert_message_falls_back_to_first_when_no_lockstep() {
        let msg = build_revert_message(
            "abcdef1234567890",
            &["mycrate-v1.0.0".into(), "other-v1.0.0".into()],
            false,
        );
        assert!(msg.starts_with("chore(release): rollback mycrate-v1.0.0 [skip ci]"));
    }

    #[test]
    fn revert_message_dry_run_marks_pending_tag_deletion() {
        let msg = build_revert_message("abcdef1234567890", &["v1.0.0".into()], true);
        assert!(
            msg.contains("Tags that WOULD be deleted: v1.0.0"),
            "dry-run preview must distinguish pending deletion: {msg}"
        );
        assert!(
            !msg.contains("\nTags deleted:"),
            "dry-run preview must NOT emit the real-run label: {msg}"
        );
    }

    #[test]
    fn per_crate_regex_rejects_leading_digit() {
        // Cargo crate names must start with a letter; the rollback
        // regex must not accept `9-foo-v1.2.3` as a per-crate tag.
        assert_eq!(classify_tag("9-foo-v1.2.3"), None);
        assert_eq!(classify_tag("0bad-v1.0.0"), None);
        // Underscore-leading is still accepted (matches cargo identifier rules).
        assert_eq!(classify_tag("_foo-v1.2.3"), Some(TagKind::PerCrate));
    }

    #[test]
    fn safety_check_prefix_admits_anodize_revert_only() {
        // anodize's own prior revert subject — admissible.
        let anodize_subject = "Revert \"chore(release): rollback v1.2.3 [skip ci]\"";
        assert!(
            anodize_subject.starts_with(ANODIZE_REVERT_SUBJECT_PREFIX.as_str()),
            "anodize-generated revert must be recognised"
        );
        // GitHub's "Revert this PR" button subject — must NOT be admitted.
        let github_subject = "Revert \"feat: add new flag\"";
        assert!(
            !github_subject.starts_with(ANODIZE_REVERT_SUBJECT_PREFIX.as_str()),
            "unrelated revert PR subjects must NOT be admitted as anodize-shaped"
        );
    }

    // -----------------------------------------------------------------
    // Fixture-repo integration tests — exercise the safety-check path
    // and dry-run no-mutation guarantee against a real tempdir git repo.
    // -----------------------------------------------------------------

    use std::path::Path;
    use std::process::Command;

    fn run_git(dir: &Path, args: &[&str]) {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "test")
                    .env("GIT_AUTHOR_EMAIL", "test@test.com")
                    .env("GIT_COMMITTER_NAME", "test")
                    .env("GIT_COMMITTER_EMAIL", "test@test.com");
                cmd
            },
            "git",
        );
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Build a repo with: initial commit -> bump commit (tagged vX.Y.Z),
    /// optionally followed by extra commits to exercise the safety check.
    fn init_bump_repo(dir: &Path, extra_commits: usize) -> String {
        run_git(dir, &["init", "-b", "master"]);
        run_git(dir, &["config", "user.email", "test@test.com"]);
        run_git(dir, &["config", "user.name", "test"]);
        std::fs::write(dir.join("README"), "init").unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", "initial"]);

        std::fs::write(dir.join("Cargo.toml"), "[package]\nversion = \"1.0.0\"\n").unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", "chore(release): v1.0.0"]);
        run_git(dir, &["tag", "v1.0.0"]);

        let bump_sha = String::from_utf8(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["rev-parse", "HEAD"]).current_dir(dir);
                    cmd
                },
                "git",
            )
            .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        for i in 0..extra_commits {
            let fname = format!("extra-{i}.txt");
            std::fs::write(dir.join(&fname), "x").unwrap();
            run_git(dir, &["add", "."]);
            run_git(dir, &["commit", "-m", &format!("feat: extra work {i}")]);
        }

        bump_sha
    }

    /// Give a fixture repo a resolvable non-github.com origin: the
    /// published-state guard's probe is inapplicable there (warn +
    /// proceed), letting tests exercise their actual subject without
    /// tripping the unresolvable-origin fail-closed refusal. The URL is
    /// never contacted — these tests run with `dry_run` / `no_push`.
    fn add_non_github_origin(dir: &Path) {
        run_git(
            dir,
            &["remote", "add", "origin", "https://gitlab.example/o/r.git"],
        );
    }

    /// Write a config with no crates.io-targeting crate, satisfying the
    /// guard's fail-closed config requirement without arming the index
    /// probe — the run-path tests below exercise git mechanics, not the
    /// probe.
    fn write_minimal_config(dir: &Path) {
        std::fs::write(dir.join(".anodizer.yaml"), "project_name: fixture\n").unwrap();
    }

    fn opts_for(dir: &Path, sha: Option<String>) -> RollbackOpts {
        let _ = dir; // cwd is process-global; the with-guard helpers below set it
        RollbackOpts {
            sha,
            dry_run: false,
            no_push: true,
            force: false,
            scope: Scope::All,
            mode: Mode::Revert,
            branch: None,
            verbose: false,
            debug: false,
            quiet: true,
        }
    }

    /// Process-wide cwd swap. Marked `serial(cwd)` — the workspace-canonical
    /// cwd serial group — so these swappers mutually exclude with every other
    /// cwd-touching test in this binary (e.g. `helpers::resolve_git_context`).
    use serial_test::serial;

    #[test]
    #[serial(cwd)]
    fn safety_check_fires_when_non_bump_commits_sit_on_top() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bump_sha = init_bump_repo(dir, 2);
        add_non_github_origin(dir);
        write_minimal_config(dir);

        let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

        let opts = opts_for(dir, Some(bump_sha));
        let err = run(opts).expect_err("safety check should fire");
        let msg = format!("{err}");
        assert!(msg.contains("cannot rollback"), "got: {msg}");
        assert!(
            msg.contains("non-bump commit"),
            "missing safety-check phrasing: {msg}"
        );
    }

    #[test]
    #[serial(cwd)]
    fn safety_check_passes_against_clean_head_at_bump_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let _bump_sha = init_bump_repo(dir, 0);
        add_non_github_origin(dir);
        write_minimal_config(dir);

        let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

        // HEAD == bump_sha; safety check trivially passes (no commits
        // between HEAD and target).
        let mut opts = opts_for(dir, None);
        opts.dry_run = true; // don't mutate the fixture
        run(opts).expect("safety check should pass at HEAD == bump commit");

        // Tag still present (dry-run guarantee).
        let tags = git::get_tags_at_head_in(dir).unwrap();
        assert_eq!(tags, vec!["v1.0.0".to_string()]);
    }

    #[test]
    #[serial(cwd)]
    fn dry_run_makes_no_mutations() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let _bump_sha = init_bump_repo(dir, 0);
        let head_before = String::from_utf8(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["rev-parse", "HEAD"]).current_dir(dir);
                    cmd
                },
                "git",
            )
            .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        add_non_github_origin(dir);
        write_minimal_config(dir);

        let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

        let mut opts = opts_for(dir, None);
        opts.dry_run = true;
        run(opts).expect("dry-run should succeed");

        // Tag still present.
        let tags = git::get_tags_at_head_in(dir).unwrap();
        assert_eq!(tags, vec!["v1.0.0".to_string()]);
        // HEAD unchanged.
        let head_after = String::from_utf8(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["rev-parse", "HEAD"]).current_dir(dir);
                    cmd
                },
                "git",
            )
            .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        assert_eq!(head_before, head_after);
    }

    #[test]
    #[serial(cwd)]
    fn no_push_skips_remote_ops_but_does_local_revert() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bump_sha = init_bump_repo(dir, 0);
        // Non-github origin only; `no_push` keeps push_branch_in from contacting it.
        add_non_github_origin(dir);
        write_minimal_config(dir);

        let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

        let opts = RollbackOpts {
            sha: None,
            dry_run: false,
            no_push: true,
            force: false,
            scope: Scope::All,
            mode: Mode::Revert,
            branch: None,
            verbose: false,
            debug: false,
            quiet: true,
        };
        run(opts).expect("no-push rollback should succeed locally");

        // Local tag gone.
        let tags = git::get_tags_at_sha_in(dir, &bump_sha).unwrap();
        assert!(
            tags.is_empty(),
            "expected no tags at bump_sha; got {tags:?}"
        );

        // Revert commit landed on top of the bump.
        let subj = git::commit_subject_in(dir, "HEAD").unwrap();
        assert!(
            subj.starts_with("chore(release): rollback v1.0.0"),
            "unexpected HEAD subject: {subj}"
        );
    }

    #[test]
    #[serial(cwd)]
    fn skips_tags_not_matching_anodize_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bump_sha = init_bump_repo(dir, 0);
        add_non_github_origin(dir);
        write_minimal_config(dir);
        // Add a non-anodize tag at the same SHA.
        run_git(dir, &["tag", "internal-release"]);

        let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

        let opts = RollbackOpts {
            sha: None,
            dry_run: false,
            no_push: true,
            force: false,
            scope: Scope::All,
            mode: Mode::Revert,
            branch: None,
            verbose: false,
            debug: false,
            quiet: true,
        };
        run(opts).expect("rollback should ignore non-anodize tag");

        // Non-anodize tag survived; anodize tag is gone.
        let surviving = git::get_tags_at_sha_in(dir, &bump_sha).unwrap();
        assert_eq!(surviving, vec!["internal-release".to_string()]);
    }

    // -----------------------------------------------------------------
    // --branch flag + detached-HEAD branch resolution.
    // -----------------------------------------------------------------

    #[test]
    fn resolve_push_branch_honors_explicit_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // No git init required — explicit branch short-circuits before
        // hitting git_output_in.
        // Explicit short-circuits before any git query; SHA is irrelevant.
        let b = resolve_push_branch(
            dir,
            "0000000000000000000000000000000000000000",
            Some("release/v9.9.9-prep"),
        )
        .unwrap();
        assert_eq!(b, "release/v9.9.9-prep");
    }

    #[test]
    fn resolve_push_branch_hard_fails_on_detached_head_without_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Build a repo whose HEAD is detached AND no branch points at
        // it: commit twice on master, then `git checkout --detach` the
        // older sha — master now points past HEAD.
        run_git(dir, &["init", "-b", "master"]);
        run_git(dir, &["config", "user.email", "t@t.com"]);
        run_git(dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a"), "1").unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", "c1"]);
        let older_sha = String::from_utf8(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["rev-parse", "HEAD"]).current_dir(dir);
                    cmd
                },
                "git",
            )
            .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        std::fs::write(dir.join("a"), "2").unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", "c2"]);
        run_git(dir, &["checkout", "--detach", &older_sha]);

        // An empty env source means the `GITHUB_REF_NAME` fallback can't
        // supply a value, then verify the hard-fail surfaces the remediation.
        let env = anodizer_core::MapEnvSource::new();

        // No remote configured → SHA-derivation returns empty, falls
        // through to get_current_branch_in, which fails on detached
        // HEAD with no env fallback → operator-friendly hard-fail.
        let err = resolve_push_branch_with_env(dir, &older_sha, None, &env).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("cannot determine branch for revert push"),
            "missing hard-fail phrasing: {msg}"
        );
        assert!(
            msg.contains("--branch <name>"),
            "hard-fail must name the remediation flag: {msg}"
        );
    }

    #[test]
    fn resolve_push_branch_hard_fails_when_github_ref_name_looks_like_tag() {
        // Same shape as above (detached HEAD with no pointing branch),
        // but GITHUB_REF_NAME is set to a tag-shaped value. The
        // is_branchlike guard in get_current_branch_in must reject it,
        // and resolve_push_branch must surface the operator-friendly
        // hard-fail (not silently push to a branch named after the tag).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        run_git(dir, &["init", "-b", "master"]);
        run_git(dir, &["config", "user.email", "t@t.com"]);
        run_git(dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a"), "1").unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", "c1"]);
        let older_sha = String::from_utf8(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["rev-parse", "HEAD"]).current_dir(dir);
                    cmd
                },
                "git",
            )
            .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        std::fs::write(dir.join("a"), "2").unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", "c2"]);
        run_git(dir, &["checkout", "--detach", &older_sha]);

        let env = anodizer_core::MapEnvSource::new().with("GITHUB_REF_NAME", "v0.4.5");

        let err = resolve_push_branch_with_env(dir, &older_sha, None, &env).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("cannot determine branch for revert push"),
            "tag-shaped GITHUB_REF_NAME must trigger the operator-facing hard-fail: {msg}"
        );
    }

    #[test]
    fn resolve_push_branch_explicit_branch_wins_over_detached_head() {
        // Even when auto-resolution would hard-fail, --branch wins.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        run_git(dir, &["init", "-b", "master"]);
        run_git(dir, &["config", "user.email", "t@t.com"]);
        run_git(dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a"), "1").unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", "c1"]);
        let older_sha = String::from_utf8(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["rev-parse", "HEAD"]).current_dir(dir);
                    cmd
                },
                "git",
            )
            .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        std::fs::write(dir.join("a"), "2").unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", "c2"]);
        run_git(dir, &["checkout", "--detach", &older_sha]);

        // --branch short-circuits before any env read, so an empty env source
        // proves the explicit flag wins regardless of `GITHUB_REF_NAME`.
        let env = anodizer_core::MapEnvSource::new();
        let b = resolve_push_branch_with_env(dir, &older_sha, Some("master"), &env).unwrap();
        assert_eq!(b, "master");
    }

    // -----------------------------------------------------------------
    // Published-release guard. Drives `check_no_published_releases`
    // with a stub `gh` script in a tempdir (no PATH mutation) against a
    // fixture repo whose origin is GitHub-shaped (local config only —
    // no network is touched; the stub answers the API call).
    // -----------------------------------------------------------------

    /// Write an executable stub standing in for the `gh` CLI.
    #[cfg(unix)]
    fn write_gh_stub(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("gh-stub");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Fixture repo with a GitHub-shaped origin so
    /// `resolve_repo_slug_in` resolves owner/repo without a network.
    fn init_github_origin_repo(dir: &Path) {
        let _ = init_bump_repo(dir, 0);
        run_git(
            dir,
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        );
    }

    fn quiet_log() -> StageLogger {
        StageLogger::new("test", Verbosity::Quiet)
    }

    /// crates.io index probe stub that must never be consulted — used by
    /// tests whose fixtures carry no repo config (no tag→crate mapping
    /// exists), pinning that the probe layer stays quiet on that path.
    fn probe_untouched(_: &str, _: &str) -> Result<bool> {
        panic!("crates.io index probe must not be consulted on this path")
    }

    /// Config with no crates.io-targeting crate: layer 2 has nothing to
    /// probe, so layer-1/3 tests exercise their subject in isolation.
    /// Named (rather than inlining `Config::default()` at call sites) so
    /// the six guard tests state the fixture's INTENT — "no cargo crate"
    /// is the property under test, not an incidental default.
    fn no_cargo_config() -> anodizer_core::config::Config {
        anodizer_core::config::Config::default()
    }

    /// Minimal in-memory repo config: one crates.io-targeting cargo crate
    /// per `(name, tag_template)` pair.
    fn config_with_cargo_crates(crates: &[(&str, &str)]) -> anodizer_core::config::Config {
        let mut config = anodizer_core::config::Config::default();
        config.crates = crates
            .iter()
            .map(|(name, tmpl)| anodizer_core::config::CrateConfig {
                name: name.to_string(),
                tag_template: tmpl.to_string(),
                publish: Some(anodizer_core::config::PublishConfig {
                    cargo: Some(anodizer_core::config::CargoPublishConfig::default()),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .collect();
        config
    }

    #[test]
    #[cfg(unix)]
    fn guard_refuses_when_release_is_published() {
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo '{"id": 1, "draft": false}'"#);

        let err =
            check_no_published_releases(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log())
                .expect_err("published release must block rollback");
        let msg = err.to_string();
        assert!(msg.contains("refusing to roll back"), "got: {msg}");
        assert!(msg.contains("v1.0.0"), "must name the blocking tag: {msg}");
        assert!(
            msg.contains("--force"),
            "must name the override flag: {msg}"
        );
        assert!(
            err.downcast_ref::<RollbackRefusal>().is_some(),
            "a published-release refusal must be typed for the failure policy"
        );
        assert!(
            msg.contains("ORPHAN"),
            "must warn the release may be an orphan of a pre-cleanup rollback: {msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn guard_allows_when_release_is_draft() {
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo '{"id": 1, "draft": true}'"#);

        check_no_published_releases(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log())
            .expect("draft release is reversible; rollback may proceed");
    }

    #[test]
    #[cfg(unix)]
    fn guard_treats_missing_draft_field_as_published() {
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo '{"id": 1}'"#);

        let err =
            check_no_published_releases(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log())
                .expect_err("a release whose draft state is unknown must block");
        assert!(err.to_string().contains("refusing to roll back"));
    }

    #[test]
    #[cfg(unix)]
    fn guard_allows_when_no_release_exists() {
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(
            tmp.path(),
            r#"echo 'gh: HTTP 404: Not Found (https://api.github.com/...)' >&2; exit 1"#,
        );

        check_no_published_releases(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log())
            .expect("404 means no release; rollback may proceed");
    }

    #[test]
    #[cfg(unix)]
    fn guard_fails_closed_on_indeterminate_probe() {
        // gh binary missing entirely — with a GitHub-shaped origin and
        // no summary, an unanswerable probe means zero evidence the
        // version is safe to destroy: refuse and point at --force.
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let missing = tmp.path().join("nonexistent-gh");

        let err = check_no_published_releases(
            tmp.path(),
            &missing,
            &["v1.0.0".to_string()],
            &quiet_log(),
        )
        .expect_err("indeterminate probe must fail closed");
        let msg = err.to_string();
        assert!(msg.contains("could not determine"), "got: {msg}");
        assert!(msg.contains("v1.0.0"), "must name the tag: {msg}");
        assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
        assert!(
            err.downcast_ref::<RollbackRefusal>().is_none(),
            "an indeterminate (transient) fail-closed is mechanical, not a \
             by-design refusal — it must NOT be typed as RollbackRefusal"
        );
    }

    #[test]
    fn guard_fails_closed_when_origin_unresolvable() {
        // No 'origin' remote at all — zero evidence either way, so the
        // guard must refuse, not warn-and-proceed.
        let tmp = tempfile::tempdir().unwrap();
        let _ = init_bump_repo(tmp.path(), 0);
        let gh = tmp.path().join("gh-never-spawned");

        let err =
            check_no_published_releases(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log())
                .expect_err("unresolvable origin must fail closed");
        let msg = err.to_string();
        assert!(msg.contains("refusing to roll back"), "got: {msg}");
        assert!(msg.contains("'origin'"), "must name the remote: {msg}");
        assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
    }

    #[test]
    fn guard_proceeds_for_resolvable_non_github_origin() {
        // Origin resolves but is not github.com-shaped — the one
        // genuinely-inapplicable case: no github.com release can exist,
        // so the guard warns and proceeds without spawning the probe.
        let tmp = tempfile::tempdir().unwrap();
        let _ = init_bump_repo(tmp.path(), 0);
        run_git(
            tmp.path(),
            &["remote", "add", "origin", "https://gitlab.com/o/r.git"],
        );
        let gh = tmp.path().join("gh-never-spawned");

        check_no_published_releases(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log())
            .expect("non-github.com origin carries no probe signal; rollback may proceed");
    }

    #[test]
    #[cfg(unix)]
    fn guard_fails_closed_on_gh_auth_error() {
        // gh present but erroring (auth/network) — same fail-closed
        // ruling as a missing gh, with the probe error surfaced.
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(
            tmp.path(),
            r#"echo 'gh: HTTP 401: Bad credentials' >&2; exit 1"#,
        );

        let err =
            check_no_published_releases(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log())
                .expect_err("auth-failed probe must fail closed");
        assert!(
            err.to_string().contains("401"),
            "must carry the probe error"
        );
    }

    #[test]
    #[serial(cwd)]
    #[cfg(unix)]
    fn run_refuses_rollback_when_release_is_published() {
        // End-to-end through `run_with_gh`: the stub `gh` reports a
        // published release for v1.0.0 → rollback must refuse before
        // any mutation (tag intact, HEAD untouched).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_github_origin_repo(dir);
        let gh = write_gh_stub(dir, r#"echo '{"id": 1, "draft": false}'"#);

        let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

        let err = run_with_gh(opts_for(dir, None), &gh)
            .expect_err("published release must refuse rollback");
        assert!(err.to_string().contains("refusing to roll back"));

        let tags = git::get_tags_at_head_in(dir).unwrap();
        assert!(
            tags.contains(&"v1.0.0".to_string()),
            "tag must survive a refused rollback; got {tags:?}"
        );
    }

    #[test]
    #[serial(cwd)]
    #[cfg(unix)]
    fn run_force_bypasses_published_release_guard() {
        // Same fixture, but --force: the guard is skipped (the stub gh
        // would refuse) and the local rollback completes. The stub
        // lives OUTSIDE the repo so the revert's dirty-tree check
        // doesn't trip on an untracked file.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_github_origin_repo(dir);
        let stub_dir = tempfile::tempdir().unwrap();
        let _gh = write_gh_stub(stub_dir.path(), r#"echo '{"id": 1, "draft": false}'"#);

        let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

        let mut opts = opts_for(dir, None);
        opts.force = true;
        run(opts).expect("--force rollback must proceed without the guard");
        let tags = git::get_tags_at_head_in(dir).unwrap();
        assert!(
            !tags.contains(&"v1.0.0".to_string()),
            "tag must be deleted under --force"
        );
    }

    // -----------------------------------------------------------------
    // GitHub release cleanup: a rolled-back tag's release belongs to the
    // aborted attempt and is deleted alongside the tag (matched by tag).
    // -----------------------------------------------------------------

    /// gh stub that records every invocation's args to `record` and
    /// answers GETs with a release object (id 7) while accepting DELETEs.
    #[cfg(unix)]
    fn write_recording_gh_stub(dir: &Path, record: &Path) -> std::path::PathBuf {
        write_gh_stub(
            dir,
            &format!(
                "echo \"$@\" >> {record}\n\
                 case \"$*\" in *DELETE*) exit 0;; *) echo '{{\"id\": 7, \"draft\": true}}';; esac",
                record = record.display()
            ),
        )
    }

    #[test]
    #[cfg(unix)]
    fn release_cleanup_deletes_release_matched_by_tag() {
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let record = tmp.path().join("gh-calls.log");
        let gh = write_recording_gh_stub(tmp.path(), &record);

        delete_release_at_tag(tmp.path(), &gh, "v1.0.0", true, &quiet_log());

        let calls = std::fs::read_to_string(&record).expect("gh must have been consulted");
        assert!(
            calls.contains("/repos/o/r/releases/tags/v1.0.0"),
            "lookup must match by THIS tag only: {calls}"
        );
        assert!(
            calls.contains("-X DELETE /repos/o/r/releases/7"),
            "must delete the release id the tag lookup returned: {calls}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn release_cleanup_noop_when_no_release_exists() {
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let record = tmp.path().join("gh-calls.log");
        let gh = write_gh_stub(
            tmp.path(),
            &format!(
                "echo \"$@\" >> {}\necho 'gh: HTTP 404: Not Found' >&2; exit 1",
                record.display()
            ),
        );

        delete_release_at_tag(tmp.path(), &gh, "v1.0.0", true, &quiet_log());

        let calls = std::fs::read_to_string(&record).expect("lookup must have run");
        assert!(
            !calls.contains("DELETE"),
            "no release means no DELETE call: {calls}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn release_cleanup_skipped_for_non_github_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = init_bump_repo(tmp.path(), 0);
        run_git(
            tmp.path(),
            &["remote", "add", "origin", "https://gitlab.com/o/r.git"],
        );
        let record = tmp.path().join("gh-calls.log");
        let gh = write_recording_gh_stub(tmp.path(), &record);

        delete_release_at_tag(tmp.path(), &gh, "v1.0.0", true, &quiet_log());

        assert!(
            !record.exists(),
            "gh must never be spawned for a non-github.com origin"
        );
    }

    /// A tag with NO run summary is not attributed to this rollback: any
    /// GitHub release it carries (a human's draft notes, a prior reversible
    /// release) must be LEFT IN PLACE, never deleted — even though the tag
    /// itself is removed.
    #[test]
    #[cfg(unix)]
    fn release_cleanup_preserves_unattributed_release() {
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let record = tmp.path().join("gh-calls.log");
        let gh = write_recording_gh_stub(tmp.path(), &record);

        let outcome = delete_release_at_tag(tmp.path(), &gh, "v1.0.0", false, &quiet_log());

        assert!(matches!(outcome, ReleaseCleanup::Cleared));
        let calls = std::fs::read_to_string(&record).expect("lookup must have run");
        assert!(
            !calls.contains("DELETE"),
            "an unattributed release must never be deleted: {calls}"
        );
    }

    /// When an OWNED release lookup succeeds but the DELETE fails, the tag is
    /// RETAINED so the rollback stays retryable, never orphaning the release
    /// under a deleted tag.
    #[test]
    #[cfg(unix)]
    fn release_cleanup_retains_tag_when_release_delete_fails() {
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let record = tmp.path().join("gh-calls.log");
        let gh = write_gh_stub(
            tmp.path(),
            &format!(
                "echo \"$@\" >> {record}\n\
                 case \"$*\" in *DELETE*) echo 'gh: HTTP 500' >&2; exit 1;; \
                 *) echo '{{\"id\": 7}}';; esac",
                record = record.display()
            ),
        );

        let outcome = delete_release_at_tag(tmp.path(), &gh, "v1.0.0", true, &quiet_log());

        assert!(
            matches!(outcome, ReleaseCleanup::Retained),
            "a failed owned-release delete must retain the tag for retry"
        );
    }

    // -----------------------------------------------------------------
    // Summary-based published-state guard: the run summary on disk is
    // the primary evidence; the gh probe is consulted only for tags
    // with no summary. Proven with gh stubs whose answer CONTRADICTS
    // the summary, so the assertion pins which source decided.
    // -----------------------------------------------------------------

    /// Write a run summary for `tag` under the repo's dist tree.
    /// `rel` is the run-dir path relative to dist (e.g. "run-v1.0.0"
    /// or "mycrate/run-mycrate-v1.0.0"), `results` the per-publisher
    /// rows. The top-level flags are computed the way the producer
    /// computes them (via the public types), so these fixtures cannot
    /// drift from the real writer's shape.
    fn write_summary(
        repo: &Path,
        rel: &str,
        tag: &str,
        irreversibly_published: bool,
        results: Vec<anodizer_stage_publish::run_summary::RunSummaryResult>,
    ) {
        use anodizer_stage_publish::run_summary::{
            DeterminismAllowlist, RunSummary, write_summary_json,
        };
        let summary = RunSummary {
            schema_version: RunSummary::CURRENT_SCHEMA_VERSION,
            anodize_version: "0.0.0-test".to_string(),
            tag: tag.to_string(),
            submitter_gated: false,
            announce_gated: false,
            publishers_succeeded: 0,
            publishers_failed: 0,
            irreversibly_published,
            failure_policy: None,
            verify_release: None,
            results,
            determinism_allowlist: DeterminismAllowlist::default(),
        };
        write_summary_json(&summary, &repo.join("dist").join(rel).join("summary.json"))
            .expect("write summary fixture");
    }

    fn summary_result(
        name: &str,
        group: anodizer_core::publish_report::PublisherGroup,
        status: &str,
    ) -> anodizer_stage_publish::run_summary::RunSummaryResult {
        anodizer_stage_publish::run_summary::RunSummaryResult {
            name: name.to_string(),
            group,
            required: true,
            status: status.to_string(),
            evidence: None,
        }
    }

    #[test]
    #[cfg(unix)]
    fn guard_refuses_when_summary_shows_irreversible_publish() {
        use anodizer_core::publish_report::PublisherGroup;
        // The gh stub answers 404 (no release — would PERMIT), so the
        // refusal can only come from the summary: the summary is the
        // primary evidence and must win.
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
        write_summary(
            tmp.path(),
            "run-v1.0.0",
            "v1.0.0",
            true,
            vec![
                summary_result("cargo", PublisherGroup::Submitter, "succeeded"),
                summary_result(
                    "chocolatey",
                    PublisherGroup::Submitter,
                    "pending-moderation",
                ),
                summary_result("github-release", PublisherGroup::Assets, "succeeded"),
            ],
        );

        let err = check_not_irreversibly_published(
            tmp.path(),
            &gh,
            &["v1.0.0".to_string()],
            &no_cargo_config(),
            &probe_untouched,
            &quiet_log(),
        )
        .expect_err("irreversible publish in the summary must block rollback");
        let msg = err.to_string();
        assert!(
            msg.contains("version burned at cargo, chocolatey"),
            "got: {msg}"
        );
        assert!(
            !msg.contains("github-release"),
            "reversible publishers must not be blamed: {msg}"
        );
        assert!(
            msg.contains("--force"),
            "must name the override flag: {msg}"
        );
        assert!(
            msg.contains("cut the NEXT version"),
            "must suggest fix-forward: {msg}"
        );
        assert!(
            err.downcast_ref::<RollbackRefusal>().is_some(),
            "a burn-evidence refusal must be typed so the failure policy \
             renders it as protection, not breakage"
        );
    }

    #[test]
    #[cfg(unix)]
    fn guard_permits_when_summary_shows_only_reversible_publishers() {
        use anodizer_core::publish_report::PublisherGroup;
        // The gh stub reports a published release (would REFUSE), but
        // the summary proves only reversible publishers landed — a
        // same-version re-cut is still possible, so rollback proceeds
        // and the probe is never consulted for this tag.
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo '{"id": 1, "draft": false}'"#);
        write_summary(
            tmp.path(),
            "run-v1.0.0",
            "v1.0.0",
            false,
            vec![
                summary_result("github-release", PublisherGroup::Assets, "succeeded"),
                summary_result("homebrew", PublisherGroup::Manager, "succeeded"),
                summary_result(
                    "cargo",
                    PublisherGroup::Submitter,
                    "skipped-submitter-gated",
                ),
            ],
        );

        check_not_irreversibly_published(
            tmp.path(),
            &gh,
            &["v1.0.0".to_string()],
            &no_cargo_config(),
            &probe_untouched,
            &quiet_log(),
        )
        .expect("reversible-only summary must permit rollback without probing GitHub");
    }

    #[test]
    #[cfg(unix)]
    fn guard_refuses_on_legacy_summary_without_the_flag() {
        // A summary written before `irreversibly_published` existed
        // (raw JSON, field absent) still blocks via the per-result
        // group/status rows.
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
        let dir = tmp.path().join("dist").join("run-v1.0.0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("summary.json"),
            r#"{
                "schema_version": 1,
                "anodize_version": "0.7.0",
                "tag": "v1.0.0",
                "submitter_gated": false,
                "announce_gated": false,
                "results": [{
                    "name": "cargo",
                    "group": "Submitter",
                    "required": true,
                    "status": "succeeded",
                    "evidence": null
                }],
                "determinism_allowlist": {"compile_time": [], "runtime": []}
            }"#,
        )
        .unwrap();

        let err = check_not_irreversibly_published(
            tmp.path(),
            &gh,
            &["v1.0.0".to_string()],
            &no_cargo_config(),
            &probe_untouched,
            &quiet_log(),
        )
        .expect_err("legacy summary with a landed Submitter must block");
        assert!(err.to_string().contains("version burned at cargo"));
    }

    #[test]
    #[cfg(unix)]
    fn guard_falls_back_to_release_probe_when_no_summary_matches_the_tag() {
        use anodizer_core::publish_report::PublisherGroup;
        // A summary exists but for a DIFFERENT tag: the guarded tag has
        // no summary evidence, so the gh probe decides — and it reports
        // a published release, so rollback refuses.
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo '{"id": 1, "draft": false}'"#);
        write_summary(
            tmp.path(),
            "run-v0.9.0",
            "v0.9.0",
            false,
            vec![summary_result(
                "github-release",
                PublisherGroup::Assets,
                "succeeded",
            )],
        );

        let err = check_not_irreversibly_published(
            tmp.path(),
            &gh,
            &["v1.0.0".to_string()],
            &no_cargo_config(),
            &probe_untouched,
            &quiet_log(),
        )
        .expect_err("unsummarized tag must fall back to the release probe");
        assert!(
            err.to_string()
                .contains("published GitHub release(s) exist")
        );
    }

    #[test]
    #[cfg(unix)]
    fn guard_reads_per_crate_summary_layout() {
        use anodizer_core::publish_report::PublisherGroup;
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
        write_summary(
            tmp.path(),
            "mycrate/run-mycrate-v1.0.0",
            "mycrate-v1.0.0",
            true,
            vec![summary_result(
                "cargo",
                PublisherGroup::Submitter,
                "succeeded",
            )],
        );

        let err = check_not_irreversibly_published(
            tmp.path(),
            &gh,
            &["mycrate-v1.0.0".to_string()],
            &no_cargo_config(),
            &probe_untouched,
            &quiet_log(),
        )
        .expect_err("per-crate summary must be found and must block");
        assert!(err.to_string().contains("version burned at cargo"));
    }

    #[test]
    #[cfg(unix)]
    fn guard_ignores_malformed_summary_and_falls_back_to_probe() {
        // Unparseable summary carries no evidence: warn, then let the
        // probe decide (404 here → rollback permitted).
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
        let dir = tmp.path().join("dist").join("run-v1.0.0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("summary.json"), "not json {").unwrap();

        check_not_irreversibly_published(
            tmp.path(),
            &gh,
            &["v1.0.0".to_string()],
            &no_cargo_config(),
            &probe_untouched,
            &quiet_log(),
        )
        .expect("malformed summary + 404 probe must permit rollback");
    }

    // -----------------------------------------------------------------
    // Global crates.io index probe (layer 2): the run summary answers a
    // per-run question, but whether a version is burned on crates.io is
    // GLOBAL state — a PRIOR run may have published it, and that run's
    // summary lives on another runner's disk.
    // -----------------------------------------------------------------

    #[test]
    fn crates_io_versions_for_tag_maps_tag_families_to_crates() {
        // The tag family prefix comes from the crate's tag_template, NOT
        // the crate name: cfgd's `crd-v...` tags belong to `cfgd-crd`.
        let config = config_with_cargo_crates(&[
            ("cfgd-crd", "crd-v{{ Version }}"),
            ("cfgd", "v{{ Version }}"),
        ]);
        assert_eq!(
            crates_io_versions_for_tag(&config, "crd-v0.5.0").probes,
            vec![("cfgd-crd".to_string(), "0.5.0".to_string())]
        );
        assert_eq!(
            crates_io_versions_for_tag(&config, "v0.5.0").probes,
            vec![("cfgd".to_string(), "0.5.0".to_string())]
        );
        let unmapped = crates_io_versions_for_tag(&config, "other-v1.0.0");
        assert!(
            unmapped.probes.is_empty() && unmapped.matched_non_crates_io == 0,
            "a tag outside every configured family maps to nothing"
        );
    }

    #[test]
    fn crates_io_versions_for_tag_lockstep_maps_every_sharing_crate() {
        // Lockstep workspaces share one `v...` family across all crates —
        // a lockstep tag must probe every crates.io-targeting crate.
        let config =
            config_with_cargo_crates(&[("core", "v{{ Version }}"), ("cli", "v{{ Version }}")]);
        assert_eq!(
            crates_io_versions_for_tag(&config, "v1.2.3").probes,
            vec![
                ("core".to_string(), "1.2.3".to_string()),
                ("cli".to_string(), "1.2.3".to_string()),
            ]
        );
    }

    #[test]
    fn crates_io_versions_for_tag_excludes_custom_registry_crates() {
        // A custom `registry:` points at a different index; the crates.io
        // probe carries no signal for it (same scoping judgment the
        // publisher's guard applies).
        let mut config = config_with_cargo_crates(&[("corp-crate", "v{{ Version }}")]);
        config.crates[0]
            .publish
            .as_mut()
            .expect("fixture publish block")
            .cargo
            .as_mut()
            .expect("fixture cargo block")
            .registry = Some("corp".to_string());
        let mapping = crates_io_versions_for_tag(&config, "v1.0.0");
        assert!(mapping.probes.is_empty());
        assert_eq!(
            mapping.matched_non_crates_io, 1,
            "the family matched — the crate just probes a different index"
        );
    }

    #[test]
    #[cfg(unix)]
    fn crates_io_probe_refuses_burned_version_despite_clean_summary() {
        use anodizer_core::publish_report::PublisherGroup;
        // The v0.5.0 attempt-#5 regression: this run's summary for
        // crd-v0.5.0 shows only reversible publishers (clean), but
        // cfgd-crd@0.5.0 is live on crates.io from a PRIOR run — the
        // per-run summary must not permit deleting a tag whose version is
        // globally burned.
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
        write_summary(
            tmp.path(),
            "cfgd-crd/run-crd-v0.5.0",
            "crd-v0.5.0",
            false,
            vec![summary_result(
                "github-release",
                PublisherGroup::Assets,
                "succeeded",
            )],
        );
        let config = config_with_cargo_crates(&[("cfgd-crd", "crd-v{{ Version }}")]);
        let probe = |name: &str, version: &str| -> Result<bool> {
            assert_eq!(
                (name, version),
                ("cfgd-crd", "0.5.0"),
                "probe must target the crate name + version the tag stamps on crates.io"
            );
            Ok(true)
        };

        let err = check_not_irreversibly_published(
            tmp.path(),
            &gh,
            &["crd-v0.5.0".to_string()],
            &config,
            &probe,
            &quiet_log(),
        )
        .expect_err("a version live on the crates.io index must refuse rollback");
        let msg = err.to_string();
        assert!(
            msg.contains("live on the crates.io index"),
            "must name the global registry state: {msg}"
        );
        assert!(
            msg.contains("cfgd-crd@0.5.0"),
            "must name the burned crate@version: {msg}"
        );
        assert!(
            msg.contains("prior attempt"),
            "must explain the source: {msg}"
        );
        assert!(
            msg.contains("cut the NEXT version"),
            "must suggest fix-forward: {msg}"
        );
        assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
        assert!(
            err.downcast_ref::<RollbackRefusal>().is_some(),
            "an index-burn refusal must be typed for the failure policy"
        );
        assert!(
            !msg.contains("No local run summary corroborates"),
            "a summarized tag's index burn is corroborated — no ownership caveat: {msg}"
        );
    }

    /// Index-only burn evidence (no run summary for the tag at all):
    /// existence on crates.io proves publication, not ownership. The refusal
    /// notes the absence of a corroborating summary — leading with the likely
    /// own-prior-run explanation and pointing at the crates.io page so the
    /// rarer foreign-ownership case can be ruled out.
    #[test]
    #[cfg(unix)]
    fn crates_io_refusal_notes_possible_squatting_without_summary() {
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
        let config = config_with_cargo_crates(&[("test-project", "v{{ Version }}")]);
        let probe = |_: &str, _: &str| -> Result<bool> { Ok(true) };

        let err = check_not_irreversibly_published(
            tmp.path(),
            &gh,
            &["v0.1.0".to_string()],
            &config,
            &probe,
            &quiet_log(),
        )
        .expect_err("index-live version must refuse rollback");
        let msg = err.to_string();
        assert!(
            msg.contains("No local run summary corroborates"),
            "uncorroborated index evidence must raise the ownership caveat: {msg}"
        );
        assert!(
            msg.contains("most likely a prior run of yours"),
            "the caveat must lead with the likely own-publish explanation, not squatting: {msg}"
        );
        assert!(
            msg.contains("https://crates.io/crates/test-project"),
            "must link the crates.io page to verify ownership: {msg}"
        );
        assert!(
            err.downcast_ref::<RollbackRefusal>().is_some(),
            "still a typed refusal"
        );
    }

    #[test]
    #[cfg(unix)]
    fn crates_io_probe_permits_absent_version_with_clean_summary() {
        use anodizer_core::publish_report::PublisherGroup;
        // Clean summary AND the version positively absent from the index:
        // nothing irreversible anywhere ⇒ rollback permitted.
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
        write_summary(
            tmp.path(),
            "run-v1.0.0",
            "v1.0.0",
            false,
            vec![summary_result(
                "github-release",
                PublisherGroup::Assets,
                "succeeded",
            )],
        );
        let config = config_with_cargo_crates(&[("mycrate", "v{{ Version }}")]);
        let probe = |_: &str, _: &str| -> Result<bool> { Ok(false) };

        check_not_irreversibly_published(
            tmp.path(),
            &gh,
            &["v1.0.0".to_string()],
            &config,
            &probe,
            &quiet_log(),
        )
        .expect("clean summary + version absent from the index must permit rollback");
    }

    #[test]
    #[cfg(unix)]
    fn crates_io_probe_unreachable_index_fails_closed() {
        use anodizer_core::publish_report::PublisherGroup;
        // The index cannot be consulted: publication state is unverifiable,
        // so the guard must refuse (fail closed) rather than gamble a
        // destructive tag delete on a transient outage.
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
        write_summary(
            tmp.path(),
            "run-v1.0.0",
            "v1.0.0",
            false,
            vec![summary_result(
                "github-release",
                PublisherGroup::Assets,
                "succeeded",
            )],
        );
        let config = config_with_cargo_crates(&[("mycrate", "v{{ Version }}")]);
        let probe =
            |_: &str, _: &str| -> Result<bool> { Err(anyhow::anyhow!("connection refused")) };

        let err = check_not_irreversibly_published(
            tmp.path(),
            &gh,
            &["v1.0.0".to_string()],
            &config,
            &probe,
            &quiet_log(),
        )
        .expect_err("an unreachable index must fail closed");
        let msg = err.to_string();
        assert!(
            msg.contains("could not be reached"),
            "must explain the index is unreachable: {msg}"
        );
        assert!(
            msg.contains("no proof the version(s) are safe to destroy"),
            "must explain publication state is unverifiable: {msg}"
        );
        assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
    }

    #[test]
    #[cfg(unix)]
    fn crates_io_probe_bails_when_tag_maps_to_no_crate() {
        // The config publishes to crates.io, but the guarded tag matches no
        // crate's tag family: the probe is blind for that tag and must fail
        // closed instead of silently narrowing itself to zero crates.
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
        let config = config_with_cargo_crates(&[("myapp", "app-v{{ Version }}")]);
        let probe = |_: &str, _: &str| -> Result<bool> {
            panic!("an unmapped tag must never reach the index probe")
        };

        let err = check_not_irreversibly_published(
            tmp.path(),
            &gh,
            &["v1.0.0".to_string()],
            &config,
            &probe,
            &quiet_log(),
        )
        .expect_err("an unmappable tag must fail closed");
        let msg = err.to_string();
        assert!(
            msg.contains("could not map these tag(s) to any crate"),
            "must name the mapping failure: {msg}"
        );
        assert!(msg.contains("v1.0.0"), "must name the tag: {msg}");
        assert!(
            msg.contains("tag_template"),
            "must point at the family mapping to fix: {msg}"
        );
        assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
    }

    #[test]
    #[cfg(unix)]
    fn crates_io_probe_proceeds_when_mapped_crates_skip_crates_io() {
        // The tag maps to a crate, but that crate publishes to a custom
        // registry: no crates.io one-way door exists for it, so the guard
        // proceeds without probing (distinct from the unmapped-tag bail).
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
        let mut config = config_with_cargo_crates(&[
            ("corp-crate", "corp-v{{ Version }}"),
            ("public-crate", "pub-v{{ Version }}"),
        ]);
        config.crates[0]
            .publish
            .as_mut()
            .expect("fixture publish block")
            .cargo
            .as_mut()
            .expect("fixture cargo block")
            .registry = Some("corp".to_string());
        let probe = |_: &str, _: &str| -> Result<bool> {
            panic!("a custom-registry crate must never reach the crates.io probe")
        };

        check_not_irreversibly_published(
            tmp.path(),
            &gh,
            &["corp-v1.0.0".to_string()],
            &config,
            &probe,
            &quiet_log(),
        )
        .expect("a mapped crate outside crates.io carries no cargo one-way door");
    }

    #[test]
    #[cfg(unix)]
    fn crates_io_probe_dedups_repeated_crate_version_probes() {
        // Under Scope::All a monorepo-prefixed and a bare tag can resolve to
        // the same crate@version; the index must be consulted once per pair.
        let tmp = tempfile::tempdir().unwrap();
        init_github_origin_repo(tmp.path());
        let gh = write_gh_stub(tmp.path(), r#"echo 'gh: HTTP 404: Not Found' >&2; exit 1"#);
        let mut config = config_with_cargo_crates(&[("mycrate", "v{{ Version }}")]);
        config.monorepo = Some(anodizer_core::config::MonorepoConfig {
            tag_prefix: Some("sub/".to_string()),
            ..Default::default()
        });
        let calls = std::cell::Cell::new(0usize);
        let probe = |name: &str, version: &str| -> Result<bool> {
            calls.set(calls.get() + 1);
            assert_eq!((name, version), ("mycrate", "1.0.0"));
            Ok(false)
        };

        check_not_irreversibly_published(
            tmp.path(),
            &gh,
            &["v1.0.0".to_string(), "sub/v1.0.0".to_string()],
            &config,
            &probe,
            &quiet_log(),
        )
        .expect("version absent from the index must permit rollback");
        assert_eq!(
            calls.get(),
            1,
            "the duplicate crate@version pair must be probed exactly once"
        );
    }

    #[test]
    #[serial(cwd)]
    fn run_without_config_fails_closed() {
        // Unparseable config: the guard cannot map tags to crates, so a
        // non-forced rollback must refuse instead of silently skipping the
        // crates.io probe (the pre-fix fail-open).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let _bump_sha = init_bump_repo(dir, 0);
        std::fs::write(dir.join(".anodizer.yaml"), "::: not yaml {").unwrap();
        add_non_github_origin(dir);

        let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

        let err = run(opts_for(dir, None)).expect_err("missing config must fail closed");
        let msg = format!("{err}");
        assert!(
            msg.contains("could not load the anodizer config"),
            "must name the config failure: {msg}"
        );
        assert!(msg.contains("--force"), "must name the escape hatch: {msg}");
        // Nothing was mutated: the tag survives the refusal.
        let tags = git::get_tags_at_head_in(dir).unwrap();
        assert_eq!(tags, vec!["v1.0.0".to_string()]);
    }

    #[test]
    #[serial(cwd)]
    #[cfg(unix)]
    fn run_force_bypasses_crates_io_probe() {
        // --force skips the whole published-state guard, index probe
        // included: with a committed config whose crate family matches the
        // tag (the probe WOULD map v1.0.0 → mycrate@1.0.0), the rollback
        // still completes without consulting any registry. Companion to
        // `run_force_bypasses_published_release_guard`, which pins the same
        // bypass for the GitHub-release layer.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join(".anodizer.yaml"),
            "crates:\n  - name: mycrate\n    path: .\n    tag_template: \"v{{ Version }}\"\n    publish:\n      cargo: {}\n",
        )
        .unwrap();
        init_github_origin_repo(dir);

        let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();

        let mut opts = opts_for(dir, None);
        opts.force = true;
        run(opts).expect("--force rollback must proceed without the crates.io probe");
        let tags = git::get_tags_at_head_in(dir).unwrap();
        assert!(
            !tags.contains(&"v1.0.0".to_string()),
            "tag must be deleted under --force"
        );
    }
}
