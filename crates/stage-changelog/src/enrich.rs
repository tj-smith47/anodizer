//! GitHub login enrichment for locally-fetched commits.
//!
//! The `git` backend (the default `changelog.use`) reads commits from
//! `git log`, which carries author names/emails but no SCM usernames. When
//! the release targets GitHub, [`LoginEnricher`] resolves each unique author
//! email to a GitHub login via the commits API so changelog lines can render
//! `@login` mentions instead of plain names. Resolution is strictly
//! best-effort: offline / unauthenticated / non-GitHub runs leave the
//! commits untouched and the renderer keeps name-based output.

use std::collections::HashMap;
use std::path::Path;

use crate::group::CommitInfo;

/// `(author_email, representative_sha) -> login` lookup signature injected
/// into [`LoginEnricher`] (boxed so tests can supply counting closures).
type LoginResolveFn<'a> = Box<dyn FnMut(&str, &str) -> Option<String> + 'a>;

/// Memoizing email→GitHub-login enricher for [`CommitInfo`] lists.
///
/// The lookup function is injected so tests never touch the network; the
/// production constructor wires it to
/// [`anodizer_core::git::commit_author_login`], which adds a second,
/// process-wide memo so independent enricher instances (e.g. the per-call
/// `bump`/`tag` changelog-sync entry points) still cost one API call per
/// unique author email per run.
pub(crate) struct LoginEnricher<'a> {
    /// `(author_email, representative_sha) -> login` lookup.
    resolve: LoginResolveFn<'a>,
    /// Run-local memo: one `resolve` call per unique email, failures included.
    cache: HashMap<String, Option<String>>,
}

impl<'a> LoginEnricher<'a> {
    /// Build an enricher around an injected lookup function.
    pub(crate) fn new(resolve: LoginResolveFn<'a>) -> Self {
        Self {
            resolve,
            cache: HashMap::new(),
        }
    }

    /// Production enricher resolving via the GitHub commits API for
    /// `owner/repo`. The token is REQUIRED by contract: enrichment without an
    /// explicit token is skipped entirely (never attempted via ambient `gh`
    /// auth), keeping unauthenticated runs — and the test suite — fully
    /// offline and deterministic.
    pub(crate) fn for_github_repo(
        owner: String,
        repo: String,
        token: String,
    ) -> LoginEnricher<'static> {
        LoginEnricher::new(Box::new(move |email, sha| {
            anodizer_core::git::commit_author_login(&owner, &repo, email, sha, Some(&token))
        }))
    }

    /// Fill `login` on every commit that lacks one, resolving each unique
    /// author email at most once (the first commit's SHA is the
    /// representative for that email). Unresolvable emails are left empty so
    /// the renderer's name-based fallback stays byte-identical.
    pub(crate) fn enrich(&mut self, commits: &mut [CommitInfo]) {
        for commit in commits.iter_mut() {
            if !commit.login.is_empty()
                || commit.author_email.is_empty()
                || commit.full_hash.is_empty()
            {
                continue;
            }
            let resolved = match self.cache.get(&commit.author_email) {
                Some(hit) => hit.clone(),
                None => {
                    let looked_up = (self.resolve)(&commit.author_email, &commit.full_hash);
                    self.cache
                        .insert(commit.author_email.clone(), looked_up.clone());
                    looked_up
                }
            };
            if let Some(login) = resolved {
                commit.login = login;
            }
        }
    }
}

/// Whether `use_source` can meaningfully carry GitHub login enrichment:
/// the local-git backend and the GitHub compare backend (whose noreply /
/// unlinked authors may still come back login-less). GitLab/Gitea logins
/// live in a different namespace, so enriching them from GitHub would be
/// wrong even when a github.com remote exists.
pub(crate) fn use_source_supports_github_logins(use_source: &str) -> bool {
    matches!(use_source, "git" | "github")
}

/// A usable, non-templated `release.github` target. Template placeholders
/// can't be resolved here (no render context), so they fall through to the
/// git-remote derivation.
fn usable_target(owner: &str, name: &str) -> Option<(String, String)> {
    if owner.is_empty() || name.is_empty() || owner.contains("{{") || name.contains("{{") {
        return None;
    }
    Some((owner.to_string(), name.to_string()))
}

/// Derive the GitHub `(owner, repo)` the changelog should resolve logins
/// against: an explicitly configured `release.github` wins; otherwise the
/// `origin` remote, when it parses as a github.com URL. `None` (non-GitHub
/// remote, no remote, not a repo) disables enrichment entirely.
pub(crate) fn derive_github_target(
    configured: Option<(&str, &str)>,
    workspace_root: &Path,
) -> Option<(String, String)> {
    if let Some((owner, name)) = configured
        && let Some(target) = usable_target(owner, name)
    {
        return Some(target);
    }
    anodizer_core::git::detect_github_repo_in(workspace_root).ok()
}

/// Read the first usable `crates[].release.github` (or
/// `workspaces[].crates[].release.github`) target straight from
/// `.anodizer.yaml`, for the config-less write path (`bump` / `tag`
/// changelog sync) that never builds a full release `Context`. A lightweight
/// raw read, mirroring `render::load_scope_inputs` — the engine crate cannot
/// pull in the full CLI config loader.
pub(crate) fn configured_github_target(workspace_root: &Path) -> Option<(String, String)> {
    let cfg_path = workspace_root.join(".anodizer.yaml");
    let text = std::fs::read_to_string(&cfg_path).ok()?;
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).ok()?;

    let crate_target = |c: &serde_yaml_ng::Value| -> Option<(String, String)> {
        let gh = c.get("release")?.get("github")?;
        let owner = gh.get("owner")?.as_str()?;
        let name = gh.get("name")?.as_str()?;
        usable_target(owner, name)
    };

    if let Some(crates) = raw.get("crates").and_then(|c| c.as_sequence())
        && let Some(target) = crates.iter().find_map(crate_target)
    {
        return Some(target);
    }
    if let Some(workspaces) = raw.get("workspaces").and_then(|w| w.as_sequence()) {
        for ws in workspaces {
            if let Some(crates) = ws.get("crates").and_then(|c| c.as_sequence())
                && let Some(target) = crates.iter().find_map(crate_target)
            {
                return Some(target);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(email: &str, sha: &str, login: &str) -> CommitInfo {
        CommitInfo {
            author_email: email.to_string(),
            full_hash: sha.to_string(),
            login: login.to_string(),
            ..Default::default()
        }
    }

    /// One lookup per unique email: three commits across two emails cost
    /// exactly two resolve calls, and every commit of a resolved email gets
    /// the login.
    #[test]
    fn enrich_memoizes_one_lookup_per_unique_email() {
        let mut calls: Vec<(String, String)> = Vec::new();
        let mut commits = vec![
            commit("ada@example.com", "a1a1", ""),
            commit("bo@example.com", "b2b2", ""),
            commit("ada@example.com", "c3c3", ""),
        ];
        {
            let mut enricher = LoginEnricher::new(Box::new(|email, sha| {
                calls.push((email.to_string(), sha.to_string()));
                match email {
                    "ada@example.com" => Some("ada".to_string()),
                    _ => Some("bo".to_string()),
                }
            }));
            enricher.enrich(&mut commits);
        }
        assert_eq!(
            calls,
            vec![
                ("ada@example.com".to_string(), "a1a1".to_string()),
                ("bo@example.com".to_string(), "b2b2".to_string()),
            ],
            "exactly one lookup per unique email, keyed to the first commit's SHA"
        );
        assert_eq!(commits[0].login, "ada");
        assert_eq!(commits[1].login, "bo");
        assert_eq!(
            commits[2].login, "ada",
            "memoized hit fills later commits too"
        );
    }

    /// A failed lookup is memoized as a failure — the same email is never
    /// retried, and the commit keeps an empty login for the renderer's
    /// name-based fallback.
    #[test]
    fn enrich_caches_failed_lookups() {
        let mut calls = 0usize;
        let mut commits = vec![
            commit("ghost@example.com", "a1a1", ""),
            commit("ghost@example.com", "b2b2", ""),
        ];
        {
            let mut enricher = LoginEnricher::new(Box::new(|_, _| {
                calls += 1;
                None
            }));
            enricher.enrich(&mut commits);
        }
        assert_eq!(calls, 1, "failure must be cached, not retried");
        assert!(commits.iter().all(|c| c.login.is_empty()));
    }

    /// Commits that already carry a login (SCM API backends) are left
    /// untouched and cost no lookup; ditto commits missing email or SHA.
    #[test]
    fn enrich_skips_resolved_and_unresolvable_commits() {
        let mut calls = 0usize;
        let mut commits = vec![
            commit("ada@example.com", "a1a1", "already-resolved"),
            commit("", "b2b2", ""),
            commit("no-sha@example.com", "", ""),
        ];
        {
            let mut enricher = LoginEnricher::new(Box::new(|_, _| {
                calls += 1;
                Some("never".to_string())
            }));
            enricher.enrich(&mut commits);
        }
        assert_eq!(calls, 0);
        assert_eq!(commits[0].login, "already-resolved");
        assert!(commits[1].login.is_empty());
        assert!(commits[2].login.is_empty());
    }

    /// The cache persists across `enrich` calls on the same instance, so one
    /// enricher shared across a multi-crate run costs one lookup per unique
    /// email across ALL crates, not per crate.
    #[test]
    fn enrich_shares_cache_across_calls() {
        let mut calls = 0usize;
        let mut crate_a = vec![commit("ada@example.com", "a1a1", "")];
        let mut crate_b = vec![commit("ada@example.com", "b2b2", "")];
        {
            let mut enricher = LoginEnricher::new(Box::new(|_, _| {
                calls += 1;
                Some("ada".to_string())
            }));
            enricher.enrich(&mut crate_a);
            enricher.enrich(&mut crate_b);
        }
        assert_eq!(calls, 1, "second crate must reuse the first crate's lookup");
        assert_eq!(crate_a[0].login, "ada");
        assert_eq!(crate_b[0].login, "ada");
    }

    /// Configured `release.github` wins over the git remote; templated or
    /// partial targets fall through.
    #[test]
    fn derive_prefers_usable_configured_target() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            derive_github_target(Some(("octo", "repo")), tmp.path()),
            Some(("octo".to_string(), "repo".to_string()))
        );
        // Templated owner → unusable; tmpdir is not a git repo → None.
        assert_eq!(
            derive_github_target(Some(("{{ .Env.OWNER }}", "repo")), tmp.path()),
            None
        );
        assert_eq!(derive_github_target(Some(("", "repo")), tmp.path()), None);
        assert_eq!(derive_github_target(None, tmp.path()), None);
    }

    /// `use: gitlab` / `use: gitea` logins live in a different namespace and
    /// must never be backfilled from GitHub.
    #[test]
    fn scm_namespace_gate() {
        assert!(use_source_supports_github_logins("git"));
        assert!(use_source_supports_github_logins("github"));
        assert!(!use_source_supports_github_logins("gitlab"));
        assert!(!use_source_supports_github_logins("gitea"));
        assert!(!use_source_supports_github_logins("github-native"));
    }

    /// The raw-yaml read finds the first usable `release.github` in flat
    /// `crates:` and in nested `workspaces[].crates`.
    #[test]
    fn configured_target_reads_flat_and_nested_crates() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".anodizer.yaml"),
            "crates:\n  - name: a\n    path: .\n    release:\n      github:\n        owner: octo\n        name: repo\n",
        )
        .unwrap();
        assert_eq!(
            configured_github_target(tmp.path()),
            Some(("octo".to_string(), "repo".to_string()))
        );

        std::fs::write(
            tmp.path().join(".anodizer.yaml"),
            "workspaces:\n  - name: ws\n    crates:\n      - name: a\n        path: crates/a\n        release:\n          github:\n            owner: nested\n            name: deep\n",
        )
        .unwrap();
        assert_eq!(
            configured_github_target(tmp.path()),
            Some(("nested".to_string(), "deep".to_string()))
        );

        std::fs::write(tmp.path().join(".anodizer.yaml"), "crates: []\n").unwrap();
        assert_eq!(configured_github_target(tmp.path()), None);
    }
}
