use super::types::{RollbackRefusal, refusal_next_step};
use anodizer_core::git;
use anodizer_core::log::StageLogger;
use anyhow::{Result, bail};

/// Outcome of probing GitHub for a release at a tag.
#[derive(Debug)]
pub(super) enum ReleaseProbe {
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
pub(super) fn probe_release_for_tag(
    gh_binary: &std::path::Path,
    owner: &str,
    repo: &str,
    tag: &str,
    redact_env: &[(String, String)],
) -> ReleaseProbe {
    let endpoint = format!("/repos/{owner}/{repo}/releases/tags/{tag}");
    match git::gh_api_get_with_binary_with_env(gh_binary, &endpoint, None, redact_env) {
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
pub(super) fn check_no_published_releases(
    cwd: &std::path::Path,
    gh_binary: &std::path::Path,
    tags: &[String],
    log: &StageLogger,
    redact_env: &[(String, String)],
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
        match probe_release_for_tag(gh_binary, &owner, &repo, tag, redact_env) {
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
