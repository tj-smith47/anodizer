//! Build-time guard that converts a silent "no binary produced" failure
//! into an immediate, actionable error.
//!
//! A crate can configure publishers and packagers that *require* a
//! compiled binary (container images, the binary-consuming publisher
//! manifests, Linux packages, OS installers, ...). If the build produces
//! no binary artifact for that crate — a mis-scoped release, an empty
//! `build.targets`, a wrong crate path — every binary-consuming stage
//! quietly logs `(no binaries, skipped)` and the release proceeds with a
//! source-only dist. The mismatch then detonates ~20 minutes later inside
//! the publish or container phase.
//!
//! [`check`] runs at the point in the pipeline where the binary artifact
//! set for each in-scope crate is known, and errors when a crate declares
//! a binary-requiring surface but no compiled-binary artifact
//! ([`ArtifactKind::Binary`] / [`ArtifactKind::UniversalBinary`], or the
//! per-target [`ArtifactKind::Archive`] that wraps one) exists for it.

use crate::artifact::{ArtifactKind, ArtifactRegistry};
use crate::config::{Config, CrateConfig, PublishConfig};
use std::collections::HashSet;

/// Artifact kinds that count as "a compiled binary is present for this
/// crate". A per-target [`ArtifactKind::Archive`] wraps a binary, so its
/// presence also satisfies the guard — the archive stage only emits one
/// when it had a binary to pack.
const BINARY_PRESENCE_KINDS: &[ArtifactKind] = &[
    ArtifactKind::Binary,
    ArtifactKind::UploadableBinary,
    ArtifactKind::UniversalBinary,
    ArtifactKind::Archive,
];

/// Validate that every in-scope crate which configures a binary-requiring
/// surface actually has a compiled-binary artifact in `artifacts`.
///
/// `selected_crates` follows the pipeline-wide convention: empty means
/// "all configured crates are in scope"; otherwise only the named crates
/// are checked.
///
/// `built_crate_names` makes the check target-aware:
/// - `None` — the build stage did not run in this pipeline (merge mode,
///   where binaries are pre-loaded); every in-scope crate is checked.
/// - `Some(set)` — the build stage ran and `set` names the crates that had
///   at least one in-scope build target. A crate absent from `set` had no
///   in-scope target in this shard (e.g. a Linux-only crate on a Windows
///   determinism shard) and is skipped — it is not this shard's
///   responsibility. A crate present in `set` but with no binary artifact
///   still fails: it was built yet produced nothing (the real mis-scope the
///   guard exists to catch).
///
/// Returns the first offending crate as an error (named, with the
/// configured surfaces and the likely causes) so the release aborts at
/// build time instead of mid-publish.
pub fn check(
    config: &Config,
    artifacts: &ArtifactRegistry,
    selected_crates: &[String],
    built_crate_names: Option<&HashSet<String>>,
) -> anyhow::Result<()> {
    for krate in &config.crates {
        if !selected_crates.is_empty() && !selected_crates.contains(&krate.name) {
            continue;
        }

        // Target-aware skip: the build stage ran but produced no in-scope
        // build target for this crate, so it was never this shard's job to
        // build it. Absent only when `built_crate_names` is `Some`.
        if let Some(built) = built_crate_names
            && !built.contains(&krate.name)
        {
            continue;
        }

        let surfaces = binary_requiring_surfaces(krate);
        if surfaces.is_empty() {
            continue;
        }

        let has_binary = !artifacts
            .by_kinds_and_crate(BINARY_PRESENCE_KINDS, &krate.name)
            .is_empty();
        if has_binary {
            continue;
        }

        anyhow::bail!(
            "release: crate '{}' configures {}, which require a compiled binary, \
             but the build produced no binary artifacts for it — check build.targets \
             and that the release is scoped to the right crate",
            krate.name,
            surfaces.join(" + "),
        );
    }
    Ok(())
}

/// Collect the names of the binary-requiring surfaces configured on a
/// single crate, in a stable declaration order. Empty when the crate is
/// library-shaped (only `source:`-style / non-binary publishers, or no
/// binary-consuming packagers at all).
///
/// Only per-crate surfaces are considered: project-wide `makeselfs:` and
/// `homebrew_casks:` bind to crates by id / artifact filter rather than a
/// crate name, so attributing their binary requirement to one specific
/// crate would risk a false positive and they are intentionally excluded.
fn binary_requiring_surfaces(krate: &CrateConfig) -> Vec<&'static str> {
    let mut surfaces = Vec::new();

    if has_entries(&krate.docker_v2) {
        surfaces.push("docker_v2");
    }
    if has_entries(&krate.nfpms) {
        surfaces.push("nfpm");
    }
    if has_entries(&krate.snapcrafts) {
        surfaces.push("snapcraft");
    }
    if has_entries(&krate.dmgs) {
        surfaces.push("dmg");
    }
    if has_entries(&krate.msis) {
        surfaces.push("msi");
    }
    if has_entries(&krate.pkgs) {
        surfaces.push("pkg");
    }
    if has_entries(&krate.nsis) {
        surfaces.push("nsis");
    }
    if has_entries(&krate.flatpaks) {
        surfaces.push("flatpak");
    }
    if has_entries(&krate.app_bundles) {
        surfaces.push("app_bundle");
    }

    if let Some(publish) = &krate.publish {
        surfaces.extend(binary_requiring_publishers(publish));
    }

    surfaces
}

/// Names of the binary-consuming publishers configured under a crate's
/// `publish:` block.
///
/// Excludes source-distributing publishers (`cargo`, `aur_source`) whose
/// output needs no compiled binary.
fn binary_requiring_publishers(publish: &PublishConfig) -> Vec<&'static str> {
    let mut names = Vec::new();
    if publish.homebrew.is_some() {
        names.push("homebrew");
    }
    if publish.homebrew_cask.is_some() {
        names.push("homebrew_cask");
    }
    if publish.scoop.is_some() {
        names.push("scoop");
    }
    if publish.chocolatey.is_some() {
        names.push("chocolatey");
    }
    if publish.winget.is_some() {
        names.push("winget");
    }
    names
}

/// `true` when an optional list field carries at least one entry. A
/// present-but-empty list (`docker_v2: []`) declares no surface and must
/// not arm the guard.
fn has_entries<T>(field: &Option<Vec<T>>) -> bool {
    field.as_ref().is_some_and(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::Artifact;
    use crate::config::{DockerV2Config, ScoopConfig};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn crate_named(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            ..CrateConfig::default()
        }
    }

    fn binary_artifact(crate_name: &str) -> Artifact {
        Artifact {
            kind: ArtifactKind::Binary,
            path: PathBuf::from(format!("dist/{crate_name}")),
            name: crate_name.to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: crate_name.to_string(),
            metadata: HashMap::new(),
            size: None,
        }
    }

    fn source_artifact(crate_name: &str) -> Artifact {
        Artifact {
            kind: ArtifactKind::SourceArchive,
            path: PathBuf::from(format!("dist/{crate_name}.tar.gz")),
            name: format!("{crate_name}.tar.gz"),
            target: None,
            crate_name: crate_name.to_string(),
            metadata: HashMap::new(),
            size: None,
        }
    }

    fn config_with(krate: CrateConfig) -> Config {
        Config {
            crates: vec![krate],
            ..Config::default()
        }
    }

    #[test]
    fn errors_when_binary_surface_configured_but_no_binary() {
        let mut krate = crate_named("svc");
        krate.docker_v2 = Some(vec![DockerV2Config::default()]);
        krate.publish = Some(PublishConfig {
            scoop: Some(ScoopConfig::default()),
            ..PublishConfig::default()
        });
        let config = config_with(krate);

        let mut artifacts = ArtifactRegistry::new();
        artifacts.add(source_artifact("svc"));

        let err = check(&config, &artifacts, &[], None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("crate 'svc'"), "{err}");
        assert!(err.contains("docker_v2"), "{err}");
        assert!(err.contains("scoop"), "{err}");
        assert!(err.contains("no binary artifacts"), "{err}");
    }

    #[test]
    fn ok_for_library_crate_with_no_binary_surface() {
        // Only a source archive, no binary-requiring surface configured —
        // the inverse of the failure case: must NOT trip the guard.
        let config = config_with(crate_named("libonly"));

        let mut artifacts = ArtifactRegistry::new();
        artifacts.add(source_artifact("libonly"));

        check(&config, &artifacts, &[], None).expect("library crate must pass");
    }

    #[test]
    fn ok_when_binary_surface_has_binary() {
        let mut krate = crate_named("svc");
        krate.docker_v2 = Some(vec![DockerV2Config::default()]);
        let config = config_with(krate);

        let mut artifacts = ArtifactRegistry::new();
        artifacts.add(binary_artifact("svc"));

        check(&config, &artifacts, &[], None).expect("binary present must pass");
    }

    #[test]
    fn ok_when_archive_wraps_binary() {
        // A per-target Archive (the binary's package) satisfies the guard
        // even with no raw Binary entry — the archive stage only emits one
        // when it had a binary to pack.
        let mut krate = crate_named("svc");
        krate.docker_v2 = Some(vec![DockerV2Config::default()]);
        let config = config_with(krate);

        let mut artifacts = ArtifactRegistry::new();
        artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: PathBuf::from("dist/svc.tar.gz"),
            name: "svc.tar.gz".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "svc".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        check(&config, &artifacts, &[], None).expect("archive-wrapped binary must pass");
    }

    #[test]
    fn empty_surface_list_does_not_arm_guard() {
        // `docker_v2: []` is present-but-empty: it declares no surface and
        // must not fire even though the field is `Some`.
        let mut krate = crate_named("svc");
        krate.docker_v2 = Some(vec![]);
        let config = config_with(krate);

        let mut artifacts = ArtifactRegistry::new();
        artifacts.add(source_artifact("svc"));

        check(&config, &artifacts, &[], None).expect("empty surface list must pass");
    }

    #[test]
    fn out_of_scope_crate_is_not_checked() {
        // The offending crate is configured but not selected; the guard
        // only checks in-scope crates.
        let mut bad = crate_named("svc");
        bad.docker_v2 = Some(vec![DockerV2Config::default()]);
        let config = config_with(bad);

        let mut artifacts = ArtifactRegistry::new();
        artifacts.add(source_artifact("svc"));

        check(&config, &artifacts, &["other".to_string()], None)
            .expect("out-of-scope crate must not be checked");
    }

    #[test]
    fn skips_crate_absent_from_built_set() {
        // The cfgd-csi-on-macOS case: the crate configures docker_v2 but had
        // no in-scope build target in this shard, so the build stage never
        // built it. It must NOT be this shard's responsibility — skip it.
        let mut krate = crate_named("cfgd-csi");
        krate.docker_v2 = Some(vec![DockerV2Config::default()]);
        let config = config_with(krate);

        let artifacts = ArtifactRegistry::new();
        let built: HashSet<String> = ["cfgd".to_string()].into_iter().collect();

        check(&config, &artifacts, &[], Some(&built))
            .expect("crate with no in-scope target must be skipped");
    }

    #[test]
    fn bails_when_built_crate_has_no_binary() {
        // The real mis-scope: the crate WAS built (present in the built set)
        // yet produced no binary artifact. The guard must still fire.
        let mut krate = crate_named("svc");
        krate.docker_v2 = Some(vec![DockerV2Config::default()]);
        let config = config_with(krate);

        let mut artifacts = ArtifactRegistry::new();
        artifacts.add(source_artifact("svc"));
        let built: HashSet<String> = ["svc".to_string()].into_iter().collect();

        let err = check(&config, &artifacts, &[], Some(&built))
            .unwrap_err()
            .to_string();
        assert!(err.contains("crate 'svc'"), "{err}");
        assert!(err.contains("no binary artifacts"), "{err}");
    }

    #[test]
    fn none_built_set_still_bails_on_missing_binary() {
        // Merge-mode call site passes `None`: every in-scope crate is checked,
        // preserving the original bail-on-no-binary behavior.
        let mut krate = crate_named("svc");
        krate.docker_v2 = Some(vec![DockerV2Config::default()]);
        let config = config_with(krate);

        let mut artifacts = ArtifactRegistry::new();
        artifacts.add(source_artifact("svc"));

        let err = check(&config, &artifacts, &[], None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("crate 'svc'"), "{err}");
        assert!(err.contains("no binary artifacts"), "{err}");
    }
}
