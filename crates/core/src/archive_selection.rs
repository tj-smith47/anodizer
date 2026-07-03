//! Which crates the archive stage will produce archives for.
//!
//! The archive stage's multi-vs-single decision (`work.len() > 1`) drives
//! per-crate `ProjectName` rebinding and the default `name_template`
//! selection, and name-deriving consumers (cargo-binstall `pkg_url`
//! derivation, the remote installer) must reproduce the exact asset names
//! the stage uploads. This module owns the one selection predicate both
//! sides resolve through, so a consumer's naming decision can never drift
//! from the stage's real work list (the "binstall 404" class).

use crate::artifact::{ArtifactKind, ArtifactRegistry};
use crate::config::{ArchivesConfig, Config, CrateConfig};

/// Artifact kinds eligible for archiving: binaries, universal binaries,
/// C headers, C static archives, and C shared libraries.
pub const ARCHIVABLE_KINDS: &[ArtifactKind] = &[
    ArtifactKind::Binary,
    ArtifactKind::UniversalBinary,
    ArtifactKind::Header,
    ArtifactKind::CArchive,
    ArtifactKind::CShared,
];

/// The crates the archive stage will produce archives for this run: every
/// [`Config::crate_universe`] crate that passes the `selected` scope filter
/// (empty = all) and has something to archive — configured builds, a
/// meta-archive, or already-registered archivable artifacts. `archives:
/// false` excludes a crate outright.
///
/// The builds / meta components are pure config, so they answer identically
/// at any pipeline point; the registered-artifact component reflects
/// `artifacts` as passed, which callers earlier in the pipeline than the
/// archive stage see partially populated. That only affects crates with no
/// configured builds and no meta-archive (artifact-only crates, e.g. merge
/// mode), where the deriving callers run after artifacts are loaded.
pub fn archive_producing_crates<'a>(
    config: &'a Config,
    artifacts: &ArtifactRegistry,
    selected: &[String],
) -> Vec<&'a CrateConfig> {
    config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter(|c| match &c.archives {
            ArchivesConfig::Disabled => false,
            ArchivesConfig::Configs(cfgs) => {
                let has_builds = c.builds.as_ref().is_some_and(|b| !b.is_empty());
                let has_meta_archive = cfgs.iter().any(|cfg| cfg.meta.unwrap_or(false));
                let has_existing_artifacts = !artifacts
                    .by_kinds_and_crate(ARCHIVABLE_KINDS, &c.name)
                    .is_empty();
                has_builds || has_meta_archive || has_existing_artifacts
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BuildConfig, WorkspaceConfig};

    fn crate_with_build(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            builds: Some(vec![BuildConfig::default()]),
            ..CrateConfig::default()
        }
    }

    #[test]
    fn workspace_crate_counts_toward_multi_crate_selection() {
        // 1 root crate + 1 workspace crate must select BOTH, so the
        // multi-vs-single naming decision matches the archive stage's
        // real work list rather than the top-level `crates:` length.
        let config = Config {
            crates: vec![crate_with_build("root")],
            workspaces: Some(vec![WorkspaceConfig {
                name: "grp".to_string(),
                crates: vec![crate_with_build("member")],
                ..WorkspaceConfig::default()
            }]),
            ..Config::default()
        };
        let artifacts = ArtifactRegistry::new();

        let producing = archive_producing_crates(&config, &artifacts, &[]);
        let names: Vec<&str> = producing.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["root", "member"]);
    }

    #[test]
    fn disabled_archives_and_nothing_to_archive_are_excluded() {
        let config = Config {
            crates: vec![
                crate_with_build("bin"),
                CrateConfig {
                    name: "off".to_string(),
                    builds: Some(vec![BuildConfig::default()]),
                    archives: ArchivesConfig::Disabled,
                    ..CrateConfig::default()
                },
                // No builds, no meta, no registered artifacts: nothing to
                // archive, so it must not count toward the multi decision.
                CrateConfig {
                    name: "lib".to_string(),
                    ..CrateConfig::default()
                },
            ],
            ..Config::default()
        };
        let artifacts = ArtifactRegistry::new();

        let producing = archive_producing_crates(&config, &artifacts, &[]);
        let names: Vec<&str> = producing.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["bin"]);
    }

    #[test]
    fn selected_scope_filters_the_work_list() {
        let config = Config {
            crates: vec![crate_with_build("a"), crate_with_build("b")],
            ..Config::default()
        };
        let artifacts = ArtifactRegistry::new();

        let producing = archive_producing_crates(&config, &artifacts, &["b".to_string()]);
        let names: Vec<&str> = producing.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["b"]);
    }
}
