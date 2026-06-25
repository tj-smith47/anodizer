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
    /// registry that never accepts the same version twice — or, when no
    /// summary exists, when the tag's GitHub release is published
    /// (non-draft).
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
    let mut body = format!("chore(release): rollback {primary} [skip ci]\n\nReverts {short}.",);
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

/// Prefix that anodize's own `build_revert_message` always produces.
/// Used by the rollback safety check to recognise its own prior revert
/// commit (so re-runs are idempotent) without absorbing unrelated
/// `Revert "<...>"` commits that GitHub's "Revert this PR" button emits
/// with arbitrary upstream subjects.
const ANODIZE_REVERT_SUBJECT_PREFIX: &str = "Revert \"chore(release): ";

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
    if opts.force {
        log.warn("skipped the published-state guard — --force");
    } else {
        check_not_irreversibly_published(&cwd, gh_binary, &deletable, &log)?;
    }

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
            if subject.starts_with(ANODIZE_REVERT_SUBJECT_PREFIX)
                || subject.starts_with("chore(release): rollback")
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
        delete_tags(&cwd, &deletable, &opts, &log);
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
        delete_tags(&cwd, &deletable, &opts, &log);
        log.status("skipped branch push — --no-push");
        return Ok(());
    }
    let branch = resolve_push_branch(&cwd, &target_sha, opts.branch.as_deref())?;
    if opts.dry_run {
        log.status(&format!("(dry-run) would run: git push origin {branch}"));
        delete_tags(&cwd, &deletable, &opts, &log);
    } else {
        // Push BEFORE deleting remote tags: the destructive tag delete is the
        // last step, so a push failure aborts before any tag is dropped.
        git::push_branch_in(&cwd, &branch)?;
        log.status(&format!("pushed revert to origin/{branch}"));
        delete_tags(&cwd, &deletable, &opts, &log);
    }
    Ok(())
}

/// Per-tag delete pass: warn-and-continue per tag so a single
/// remote-delete glitch doesn't abandon the surrounding mutation.
/// `dry_run` short-circuits to a status line per tag; `no_push`
/// skips the remote leg.
fn delete_tags(
    cwd: &std::path::Path,
    deletable: &[String],
    opts: &RollbackOpts,
    log: &StageLogger,
) {
    for tag in deletable {
        if opts.dry_run {
            log.status(&format!("(dry-run) would delete tag {tag} (remote+local)"));
            continue;
        }
        if !opts.no_push {
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
///    failed runs. A summary that shows only reversible publishers
///    (github-release assets, blobs, tap/bucket/index commits)
///    PERMITS rollback: their state can be deleted and the same
///    version re-cut.
/// 2. Only for tags with no matching summary (e.g. a fresh checkout
///    that never ran the release): fall back to probing the GitHub
///    Releases API for a published (non-draft) release at the tag.
fn check_not_irreversibly_published(
    cwd: &std::path::Path,
    gh_binary: &std::path::Path,
    tags: &[String],
    log: &StageLogger,
) -> Result<()> {
    let summaries = collect_run_summaries(&resolve_dist_dir(cwd), log);
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
                "no one-way-door publisher landed for {tag} (per run summary) — rollback permitted"
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
        bail!(
            "refusing to roll back — one-way-door publisher(s) already accepted these version(s):\n\
             {detail}\n\
             Those registries never accept the same version twice, so deleting the tag(s) \
             and reverting the bump cannot lead to a clean same-version re-cut — it only \
             orphans the live published state.\n\
             Fix forward instead: keep the tag, repair the failure, and cut the NEXT version \
             (or re-run the failed stages against this tag). Pass --force to override.",
        );
    }
    if unsummarized.is_empty() {
        return Ok(());
    }
    check_no_published_releases(cwd, gh_binary, &unsummarized, log)
}

/// Best-effort dist-dir resolution for the published-state guard: the
/// repo config's `dist:` when a config is present and parseable, else
/// the default `dist`. Relative values anchor at `cwd`. Best-effort
/// because rollback is failure-recovery tooling — a broken or missing
/// config must not stop it (the guard then simply finds no summaries
/// and falls back to the GitHub release probe).
fn resolve_dist_dir(cwd: &std::path::Path) -> std::path::PathBuf {
    let dist = crate::pipeline::load_repo_config(cwd)
        .map(|c| c.dist)
        .unwrap_or_else(|_| std::path::PathBuf::from("dist"));
    if dist.is_absolute() {
        dist
    } else {
        cwd.join(dist)
    }
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
    let (owner, repo) = match git::detect_github_repo_in(cwd) {
        Ok(pair) => pair,
        Err(e) if git::has_remote_in(cwd, "origin") => {
            // `detect_github_repo_in` already redacts URL credentials in
            // its parse-failure message, so `e` is safe to surface.
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
        bail!(
            "refusing to roll back: published GitHub release(s) exist for: {} \
             (and no run summary is available to prove nothing irreversible shipped).\n\
             One-way-door publishers (crates.io, chocolatey, winget, snapcraft, ...) \
             usually ship alongside a published release; if any did, the version is \
             burned and deleting the tag(s) only orphans live published state.\n\
             Fix forward instead: keep the tag, repair the failure, and cut the NEXT \
             version. Pass --force to override the guard.",
            published.join(", ")
        );
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
            anodize_subject.starts_with(ANODIZE_REVERT_SUBJECT_PREFIX),
            "anodize-generated revert must be recognised"
        );
        // GitHub's "Revert this PR" button subject — must NOT be admitted.
        let github_subject = "Revert \"feat: add new flag\"";
        assert!(
            !github_subject.starts_with(ANODIZE_REVERT_SUBJECT_PREFIX),
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

    /// Process-wide cwd swap. Marked `serial` to match the surrounding
    /// cwd-swapping tests so they don't race.
    use serial_test::serial;

    #[test]
    #[serial]
    fn safety_check_fires_when_non_bump_commits_sit_on_top() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bump_sha = init_bump_repo(dir, 2);
        add_non_github_origin(dir);

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
    #[serial]
    fn safety_check_passes_against_clean_head_at_bump_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let _bump_sha = init_bump_repo(dir, 0);
        add_non_github_origin(dir);

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
    #[serial]
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
    #[serial]
    fn no_push_skips_remote_ops_but_does_local_revert() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bump_sha = init_bump_repo(dir, 0);
        // Non-github origin only; `no_push` keeps push_branch_in from contacting it.
        add_non_github_origin(dir);

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
    #[serial]
    fn skips_tags_not_matching_anodize_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bump_sha = init_bump_repo(dir, 0);
        add_non_github_origin(dir);
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
    /// `detect_github_repo_in` resolves owner/repo without a network.
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
    #[serial]
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
    #[serial]
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
            msg.contains("Fix forward"),
            "must suggest fix-forward: {msg}"
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

        check_not_irreversibly_published(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log())
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

        check_not_irreversibly_published(tmp.path(), &gh, &["v1.0.0".to_string()], &quiet_log())
            .expect("malformed summary + 404 probe must permit rollback");
    }
}
