use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{CommitAuthorConfig, ContentSource};

// ---------------------------------------------------------------------------
// SnapshotConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotConfig {
    /// Version string template for snapshot builds (e.g., "{{ Commit }}-SNAPSHOT").
    /// Accepts the deprecated `name_template:` alias (renamed to
    /// `version_template`): a non-empty `name_template` is folded into
    /// `version_template`.
    /// A deprecation warning is emitted at config-load time when the alias
    /// is hit (see `apply_snapshot_legacy_aliases`).
    #[serde(alias = "name_template")]
    pub version_template: String,
}

// ---------------------------------------------------------------------------
// NightlyConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NightlyConfig {
    /// Template for the rendered version string the nightly run sets on
    /// `Version` / `RawVersion`. Default:
    /// `"{{ incpatch(v=Version) }}-{{ ShortCommit }}-nightly"` — produces
    /// commit-immutable nightly versions (two same-day commits yield two
    /// distinct nightly versions).
    ///
    /// The `{{ NightlyBuild }}` template var (a stateless per-base-version
    /// build counter derived from `git rev-list --count <last-tag>..HEAD`)
    /// enables nushell-style schemes such as
    /// `"{{ Base }}-nightly.{{ NightlyBuild }}+{{ ShortCommit }}"`.
    pub version_template: Option<String>,
    /// Template for the release name. Default: `"{{ ProjectName }}-nightly"`.
    pub name_template: Option<String>,
    /// Tag name used for the nightly release. Default: `"nightly"`.
    /// Templates allowed.
    pub tag_name: Option<String>,
    /// Whether to publish a GitHub Release at all. Default: `true`.
    /// Set `false` for nightly-only docker pushes / blob uploads.
    pub publish_release: Option<bool>,
    /// Publish the nightly release to a DIFFERENT repository than the source
    /// repo, in `"owner/repo"` form (e.g. `"nushell/nightly"`). Default
    /// (`None`) publishes to the configured `release.github` repo, unchanged.
    ///
    /// When set, the nightly release create, asset upload, AND retention
    /// (`keep_single_release` / `retention.keep_last`) delete calls all
    /// target this repo. The active SCM token is assumed to have write
    /// access to `publish_repo`. GitHub-only (the nushell adoption target).
    pub publish_repo: Option<String>,
    /// Delete the prior release that points at the same tag before
    /// creating the new one. Default: `false`. Set `true` to maintain a
    /// single rolling nightly release on GitHub.
    ///
    /// Back-compat alias for `retention: { keep_last: 1 }`. When both
    /// `keep_single_release` and `retention` are set, `retention` wins.
    /// Destructive: deletes a published release via the GitHub Releases API.
    /// GitHub-only.
    pub keep_single_release: Option<bool>,
    /// Retention policy for nightly releases on GitHub. Generalizes
    /// `keep_single_release` (which is `keep_last: 1`): keeps the N newest
    /// nightly releases matching the nightly tag/name and deletes the rest
    /// (releases + the tags anodizer created for them). Operates on
    /// `publish_repo` when set. Default (`None`): no retention sweep.
    pub retention: Option<RetentionConfig>,
    /// Override `release.draft` for nightly runs only.
    /// `None` falls through to `release.draft`; `Some(v)` overrides it.
    pub draft: Option<bool>,
}

/// Retention policy for nightly releases on the publish repo.
///
/// `keep_last: N` keeps the N newest nightly releases (matched by the
/// nightly tag/name) and deletes the older ones, including the git tags
/// anodizer created for them.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct RetentionConfig {
    /// Number of newest nightly releases to keep. `0` is treated as `1`
    /// (never delete every nightly), matching the `keep_single_release`
    /// floor. nushell keeps 10.
    pub keep_last: usize,
}

impl NightlyConfig {
    /// Resolve the effective `keep_last` retention count, or `None` when no
    /// retention is requested.
    ///
    /// Precedence: an explicit `retention:` block wins over the legacy
    /// `keep_single_release:` alias. `keep_single_release: true` (with no
    /// `retention`) maps to `keep_last: 1`; `false` maps to `None`. A
    /// `retention.keep_last: 0` is floored to `1` so a retention sweep
    /// never deletes the just-created release.
    pub fn resolved_keep_last(&self) -> Option<usize> {
        if let Some(r) = self.retention.as_ref() {
            return Some(r.keep_last.max(1));
        }
        if self.keep_single_release == Some(true) {
            return Some(1);
        }
        None
    }
}

/// Validate `nightly.publish_repo` is `"owner/repo"` shaped.
///
/// Returns `Ok(())` when unset or well-formed. A malformed value (missing
/// `/`, empty owner/repo, extra path segments, or whitespace) is a
/// config-time error rather than a confusing 404 at publish time.
pub fn validate_nightly_publish_repo(config: &crate::config::Config) -> Result<(), String> {
    let Some(nightly) = config.nightly.as_ref() else {
        return Ok(());
    };
    let Some(repo) = nightly.publish_repo.as_deref() else {
        return Ok(());
    };
    let parts: Vec<&str> = repo.split('/').collect();
    let well_formed = parts.len() == 2
        && parts
            .iter()
            .all(|p| !p.trim().is_empty() && !p.contains(char::is_whitespace));
    if well_formed {
        Ok(())
    } else {
        Err(format!(
            "nightly.publish_repo must be in \"owner/repo\" form (got {repo:?})"
        ))
    }
}

// ---------------------------------------------------------------------------
// MetadataConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MetadataConfig {
    /// Human-readable project description (exposed as `{{ Metadata.Description }}`).
    pub description: Option<String>,
    /// Project homepage URL (exposed as `{{ Metadata.Homepage }}`).
    pub homepage: Option<String>,
    /// Project license identifier, e.g. "MIT" or "Apache-2.0" (exposed as `{{ Metadata.License }}`).
    pub license: Option<String>,
    /// List of project maintainers (exposed as `{{ Metadata.Maintainers }}`).
    pub maintainers: Option<Vec<String>>,
    /// Global modification timestamp for metadata output files (metadata.json and artifacts.json).
    /// Template string (e.g. "{{ CommitTimestamp }}") or unix timestamp.
    /// When set, rendered late in the pipeline and applied as file mtime.
    /// Exposed as `{{ Metadata.ModTimestamp }}`.
    pub mod_timestamp: Option<String>,
    /// Long-form project description. Supports inline
    /// string, `from_file`, or `from_url`. Exposed as `{{ Metadata.FullDescription }}`.
    /// FromUrl is resolved lazily (requires the release stage); FromFile is resolved
    /// at context-populate time with template-rendered path.
    pub full_description: Option<ContentSource>,
    /// Commit author identity for commit workflows.
    /// Reuses the shared `CommitAuthorConfig` (name + email + optional signing).
    /// Exposed as `{{ Metadata.CommitAuthor.Name }}` / `{{ Metadata.CommitAuthor.Email }}`.
    pub commit_author: Option<CommitAuthorConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn resolved_keep_last_maps_keep_single_release_alias() {
        let n = NightlyConfig {
            keep_single_release: Some(true),
            ..Default::default()
        };
        assert_eq!(n.resolved_keep_last(), Some(1));
    }

    #[test]
    fn resolved_keep_last_none_when_neither_set() {
        let n = NightlyConfig::default();
        assert_eq!(n.resolved_keep_last(), None);
        let n = NightlyConfig {
            keep_single_release: Some(false),
            ..Default::default()
        };
        assert_eq!(n.resolved_keep_last(), None);
    }

    #[test]
    fn resolved_keep_last_retention_wins_over_alias() {
        // Both set: retention.keep_last wins over the legacy alias.
        let n = NightlyConfig {
            keep_single_release: Some(true),
            retention: Some(RetentionConfig { keep_last: 10 }),
            ..Default::default()
        };
        assert_eq!(n.resolved_keep_last(), Some(10));
    }

    #[test]
    fn resolved_keep_last_floors_zero_to_one() {
        let n = NightlyConfig {
            retention: Some(RetentionConfig { keep_last: 0 }),
            ..Default::default()
        };
        assert_eq!(n.resolved_keep_last(), Some(1));
    }

    #[test]
    fn keep_single_release_yaml_alias_round_trips() {
        // Back-compat: an existing keep_single_release config still parses
        // and resolves to keep_last: 1.
        let n: NightlyConfig = serde_yaml_ng::from_str("keep_single_release: true").unwrap();
        assert_eq!(n.resolved_keep_last(), Some(1));
    }

    fn config_with_publish_repo(repo: Option<&str>) -> Config {
        Config {
            nightly: Some(NightlyConfig {
                publish_repo: repo.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn validate_publish_repo_accepts_owner_repo() {
        let c = config_with_publish_repo(Some("nushell/nightly"));
        assert!(validate_nightly_publish_repo(&c).is_ok());
    }

    #[test]
    fn validate_publish_repo_ok_when_unset() {
        assert!(validate_nightly_publish_repo(&config_with_publish_repo(None)).is_ok());
        assert!(validate_nightly_publish_repo(&Config::default()).is_ok());
    }

    #[test]
    fn validate_publish_repo_rejects_malformed() {
        for bad in ["nushell", "a/b/c", "/nightly", "nushell/", "owner repo/x"] {
            let c = config_with_publish_repo(Some(bad));
            assert!(
                validate_nightly_publish_repo(&c).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }
}
