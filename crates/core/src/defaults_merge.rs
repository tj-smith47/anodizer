//! Defaults inheritance merge engine (path-mirror).
//!
//! After WAVE 2 the workspace `defaults:` block path-mirrors the per-crate
//! `CrateConfig` (and a small subset of top-level `Config`) shape. The merge
//! engine in this module folds those defaults into every resolved crate so
//! the build pipeline can keep reading from `crate_cfg.<field>` without
//! caring whether a value was hoisted to defaults.
//!
//! ## Semantics (DEC-9)
//!
//! - **Struct-typed fields (deep-merge)**: defaults fill any field the crate
//!   left unset; on conflict, the crate value wins. Implemented via a JSON
//!   round-trip using [`serde_json::Value`] so arbitrarily nested structs
//!   merge uniformly without per-type boilerplate.
//! - **List-typed fields (append + merge-by-identity)**: each defaults
//!   entry merges into the crate entry that shares its identity key
//!   (`format` for archives, `id`/`name`/`package_name` for packagers,
//!   etc.). Defaults entries with no identity-match are appended after the
//!   crate's own entries.
//! - **Empty map at per-crate position**: written as `{}` in YAML, parses
//!   to `Some(default)` and inherits all fields from defaults.
//! - **`skip: true` at per-crate position**: suppresses the inherited
//!   block entirely (the merge engine treats the field as if defaults
//!   were `None`).
//! - **Scalar conflict**: per-crate value wins over defaults.
//!
//! ## Entry point
//!
//! [`apply_defaults`] is the single entry point; call it once per loaded
//! `Config` after deserialization (and after any include-merging) but
//! before the first stage runs.

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value as Json;

use crate::config::{
    AppBundleConfig, ArchiveConfig, ArchivesConfig, Config, CrateConfig, Defaults, DmgConfig,
    DockerV2Config, FlatpakConfig, MsiConfig, NfpmConfig, NsisConfig, PkgConfig, PublishConfig,
    PublishDefaults, SnapcraftConfig,
};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Fold workspace-level `defaults` into every per-crate config.
///
/// Mutates each `config.crates[]` (and any `config.workspaces[].crates[]`)
/// in place. Idempotent — running it twice on the same config produces the
/// same state because field-fill is a no-op once defaults have been merged
/// in, and identity-keyed list merges deduplicate by their identity key on
/// the second pass.
///
/// The merge engine reads `config.defaults`; if it is `None`, this function
/// returns immediately.
///
/// Note: top-level `Config` fields that path-mirror in `Defaults` (`source`,
/// `upx`, `sboms`, etc.) are not yet folded by this function — WAVE 2
/// scope is per-crate inheritance only. Future waves will extend the merge
/// engine to fill those fields when defaults provides them and the
/// top-level field is unset.
pub fn apply_defaults(config: &mut Config) {
    let defaults = match config.defaults.clone() {
        Some(d) => d,
        None => return,
    };

    // Per-crate merge.
    for crate_cfg in &mut config.crates {
        apply_to_crate(&defaults, crate_cfg);
    }
    // Workspace overlay crates also benefit from defaults.
    if let Some(ref mut workspaces) = config.workspaces {
        for ws in workspaces {
            for crate_cfg in &mut ws.crates {
                apply_to_crate(&defaults, crate_cfg);
            }
        }
    }
}

/// Apply defaults to a single crate. Exposed for tests; production code
/// should call [`apply_defaults`] which iterates all crates.
pub fn apply_to_crate(defaults: &Defaults, crate_cfg: &mut CrateConfig) {
    // ---- Scalar / Option<T> fields: fill if None ----
    if crate_cfg.cross.is_none() && defaults.cross.is_some() {
        crate_cfg.cross = defaults.cross.clone();
    }

    // ---- Single-struct deep-merge fields ----
    // Suppress inheritance when the per-crate block sets `skip: true` (DEC-9).
    if !checksum_skipped(crate_cfg.checksum.as_ref()) {
        deep_merge_option(&mut crate_cfg.checksum, defaults.checksum.as_ref());
    }

    // ---- List-typed fields: append + merge-by-identity ----
    merge_archives(&mut crate_cfg.archives, defaults.archives.as_ref());
    merge_list_by_identity(&mut crate_cfg.nfpm, defaults.nfpms.as_ref(), nfpm_identity);
    merge_list_by_identity(
        &mut crate_cfg.snapcrafts,
        defaults.snapcrafts.as_ref(),
        snapcraft_identity,
    );
    merge_list_by_identity(&mut crate_cfg.dmgs, defaults.dmgs.as_ref(), dmg_identity);
    merge_list_by_identity(&mut crate_cfg.pkgs, defaults.pkgs.as_ref(), pkg_identity);
    merge_list_by_identity(&mut crate_cfg.msis, defaults.msis.as_ref(), msi_identity);
    merge_list_by_identity(&mut crate_cfg.nsis, defaults.nsis.as_ref(), nsis_identity);
    merge_list_by_identity(
        &mut crate_cfg.app_bundles,
        defaults.app_bundles.as_ref(),
        app_bundle_identity,
    );
    merge_list_by_identity(
        &mut crate_cfg.flatpaks,
        defaults.flatpaks.as_ref(),
        flatpak_identity,
    );
    merge_list_by_identity(
        &mut crate_cfg.docker_v2,
        defaults.docker_v2.as_ref(),
        docker_v2_identity,
    );

    // ---- Builds: defaults.builds is a single template; fold into each
    //              crate build via deep-merge (per-build fields like
    //              flags/ignore/overrides flow through here).
    if let Some(ref tpl) = defaults.builds {
        match crate_cfg.builds.as_mut() {
            Some(list) if !list.is_empty() => {
                for entry in list {
                    deep_merge_struct_inplace(entry, tpl);
                }
            }
            _ => {
                // No per-crate builds — clone the template as a starting
                // point so downstream stages see the inherited settings.
                crate_cfg.builds = Some(vec![tpl.clone()]);
            }
        }
    }

    // ---- Publish axis: each publisher inherits its struct from
    //                    defaults.publish.<publisher> when per-crate is None.
    if let Some(ref pubd) = defaults.publish {
        let target = crate_cfg.publish.get_or_insert_with(PublishConfig::default);
        merge_publish_defaults(target, pubd);
    }
}

// ---------------------------------------------------------------------------
// Generic deep-merge helpers (JSON-backed)
// ---------------------------------------------------------------------------

/// Deep-merge two struct-typed fields when the per-crate value is `Some`.
/// On collision the per-crate value wins; defaults fill any field that the
/// per-crate value left unset (`null` after JSON serialisation).
fn deep_merge_option<T: Serialize + DeserializeOwned + Clone>(
    target: &mut Option<T>,
    defaults: Option<&T>,
) {
    let Some(defaults_val) = defaults else {
        return;
    };
    match target {
        None => {
            *target = Some(defaults_val.clone());
        }
        Some(crate_val) => {
            deep_merge_struct_inplace(crate_val, defaults_val);
        }
    }
}

/// Deep-merge `defaults` into `target` so any field the target left as
/// `null` (i.e. `None` on the original Option) is filled from defaults.
/// Other fields are left untouched.
fn deep_merge_struct_inplace<T: Serialize + DeserializeOwned>(target: &mut T, defaults: &T) {
    let Ok(mut crate_json) = serde_json::to_value(&*target) else {
        return;
    };
    let Ok(defaults_json) = serde_json::to_value(defaults) else {
        return;
    };
    deep_merge_json(&mut crate_json, &defaults_json);
    if let Ok(merged) = serde_json::from_value::<T>(crate_json) {
        *target = merged;
    }
}

/// Merge `defaults` JSON into `target` JSON in place.
///
/// - Objects: recurse field-by-field, defaults filling missing fields only
///   (target wins for any field it sets — including explicit `null`).
/// - Arrays: target wins (lists are merged by the caller using identity
///   keys, not by this generic JSON merger).
/// - Scalars: target wins; defaults only used when target is `null` /
///   missing.
fn deep_merge_json(target: &mut Json, defaults: &Json) {
    match (target, defaults) {
        (Json::Object(t), Json::Object(d)) => {
            for (k, v) in d {
                match t.get_mut(k) {
                    Some(existing) if !existing.is_null() => {
                        deep_merge_json(existing, v);
                    }
                    _ => {
                        t.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (target_slot, defaults_val) => {
            if target_slot.is_null() {
                *target_slot = defaults_val.clone();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// List merge: archives (specialised because of ArchivesConfig wrapper)
// ---------------------------------------------------------------------------

fn merge_archives(target: &mut ArchivesConfig, defaults: Option<&ArchiveConfig>) {
    let Some(default_entry) = defaults else {
        return;
    };
    match target {
        // `archives: false` at per-crate position means "no archives" — do
        // not inherit anything (suppress block).
        ArchivesConfig::Disabled => {}
        ArchivesConfig::Configs(list) => {
            merge_one_into_list(list, default_entry, archive_identity);
        }
    }
}

fn archive_identity(a: &ArchiveConfig) -> String {
    a.format.clone().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// List merge: generic "single defaults entry → Vec<T> per-crate" path
// ---------------------------------------------------------------------------

fn merge_list_by_identity<T, F>(target: &mut Option<Vec<T>>, defaults: Option<&T>, identity: F)
where
    T: Clone + Serialize + DeserializeOwned,
    F: Fn(&T) -> String,
{
    let Some(default_entry) = defaults else {
        return;
    };
    match target.as_mut() {
        Some(list) => merge_one_into_list(list, default_entry, identity),
        None => {
            *target = Some(vec![default_entry.clone()]);
        }
    }
}

/// Core "merge one defaults entry into a list" routine: if any list entry
/// shares the same identity key as the default, deep-merge it (defaults
/// fill gaps); otherwise append the default.
fn merge_one_into_list<T, F>(list: &mut Vec<T>, default_entry: &T, identity: F)
where
    T: Clone + Serialize + DeserializeOwned,
    F: Fn(&T) -> String,
{
    if list.is_empty() {
        list.push(default_entry.clone());
        return;
    }
    let default_id = identity(default_entry);
    let mut merged_into_existing = false;
    for entry in list.iter_mut() {
        if identity(entry) == default_id {
            deep_merge_struct_inplace(entry, default_entry);
            merged_into_existing = true;
        }
    }
    if !merged_into_existing {
        list.push(default_entry.clone());
    }
}

// ---------------------------------------------------------------------------
// Skip-block suppression helpers (DEC-9: `skip: true` suppresses inheritance)
// ---------------------------------------------------------------------------

fn checksum_skipped(target: Option<&crate::config::ChecksumConfig>) -> bool {
    target
        .and_then(|c| c.skip.as_ref())
        .is_some_and(crate::config::StringOrBool::as_bool)
}

// ---------------------------------------------------------------------------
// Identity functions per packaging type
// ---------------------------------------------------------------------------

fn nfpm_identity(c: &NfpmConfig) -> String {
    // GoReleaser identity: id → package_name → empty positional.
    if let Some(ref id) = c.id {
        return id.clone();
    }
    if let Some(ref pkg) = c.package_name {
        return pkg.clone();
    }
    String::new()
}

fn snapcraft_identity(c: &SnapcraftConfig) -> String {
    c.name.clone().unwrap_or_default()
}

fn dmg_identity(c: &DmgConfig) -> String {
    c.id.clone().unwrap_or_default()
}

fn pkg_identity(c: &PkgConfig) -> String {
    c.id.clone().unwrap_or_default()
}

fn msi_identity(c: &MsiConfig) -> String {
    c.id.clone().unwrap_or_default()
}

fn nsis_identity(c: &NsisConfig) -> String {
    c.id.clone().unwrap_or_default()
}

fn app_bundle_identity(c: &AppBundleConfig) -> String {
    c.id.clone().unwrap_or_default()
}

fn flatpak_identity(c: &FlatpakConfig) -> String {
    c.id.clone().unwrap_or_default()
}

fn docker_v2_identity(c: &DockerV2Config) -> String {
    c.id.clone().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Publisher defaults
// ---------------------------------------------------------------------------

fn merge_publish_defaults(target: &mut PublishConfig, defaults: &PublishDefaults) {
    deep_merge_option(&mut target.homebrew, defaults.homebrew.as_ref());
    deep_merge_option(&mut target.scoop, defaults.scoop.as_ref());
    deep_merge_option(&mut target.winget, defaults.winget.as_ref());
    deep_merge_option(&mut target.chocolatey, defaults.chocolatey.as_ref());
    deep_merge_option(&mut target.krew, defaults.krew.as_ref());
    deep_merge_option(&mut target.nix, defaults.nix.as_ref());
    deep_merge_option(&mut target.aur, defaults.aur.as_ref());
    deep_merge_option(&mut target.aur_source, defaults.aur_source.as_ref());
    // The legacy `crates` (crates.io) publisher is intentionally left
    // un-defaulted in WAVE 2 — the rename to `cargo` and its defaults
    // wiring lands in WAVE 3.
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ArchiveConfig, ArchivesConfig, ChecksumConfig, CrossStrategy, HomebrewConfig, StringOrBool,
    };

    fn make_crate(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }
    }

    // --------------- (1) Map deep-merge ---------------

    #[test]
    fn map_deep_merge_combines_disjoint_fields() {
        // defaults sets one nested field; crate sets a different nested
        // field — both should survive in the merged result.
        let defaults = Defaults {
            publish: Some(PublishDefaults {
                homebrew: Some(HomebrewConfig {
                    description: Some("hoisted-desc".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.publish = Some(PublishConfig {
            homebrew: Some(HomebrewConfig {
                license: Some("MIT".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });

        apply_to_crate(&defaults, &mut crate_cfg);

        let hb = crate_cfg.publish.unwrap().homebrew.unwrap();
        assert_eq!(hb.description, Some("hoisted-desc".to_string()));
        assert_eq!(hb.license, Some("MIT".to_string()));
    }

    // --------------- (2) List append (different identity) ---------------

    #[test]
    fn list_append_keeps_both_when_identity_differs() {
        let defaults = Defaults {
            archives: Some(ArchiveConfig {
                format: Some("tar.gz".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.archives = ArchivesConfig::Configs(vec![ArchiveConfig {
            format: Some("zip".to_string()),
            ..Default::default()
        }]);

        apply_to_crate(&defaults, &mut crate_cfg);

        if let ArchivesConfig::Configs(list) = &crate_cfg.archives {
            assert_eq!(list.len(), 2);
            let formats: Vec<_> = list.iter().map(|a| a.format.clone()).collect();
            assert!(formats.contains(&Some("tar.gz".to_string())));
            assert!(formats.contains(&Some("zip".to_string())));
        } else {
            panic!("expected Configs variant");
        }
    }

    // --------------- (3) List merge-by-identity ---------------

    #[test]
    fn list_merge_by_identity_combines_fields_crate_wins() {
        let defaults = Defaults {
            archives: Some(ArchiveConfig {
                format: Some("tar.gz".to_string()),
                name_template: Some("DEFAULT".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.archives = ArchivesConfig::Configs(vec![ArchiveConfig {
            format: Some("tar.gz".to_string()),
            // crate sets a name_template — wins over defaults
            name_template: Some("CRATE".to_string()),
            ..Default::default()
        }]);

        apply_to_crate(&defaults, &mut crate_cfg);

        if let ArchivesConfig::Configs(list) = &crate_cfg.archives {
            assert_eq!(list.len(), 1, "should merge into single entry");
            assert_eq!(list[0].name_template, Some("CRATE".to_string()));
            assert_eq!(list[0].format, Some("tar.gz".to_string()));
        } else {
            panic!("expected Configs variant");
        }
    }

    #[test]
    fn list_merge_by_identity_fills_unset_fields_from_defaults() {
        let defaults = Defaults {
            archives: Some(ArchiveConfig {
                format: Some("tar.gz".to_string()),
                name_template: Some("DEFAULT".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.archives = ArchivesConfig::Configs(vec![ArchiveConfig {
            format: Some("tar.gz".to_string()),
            // crate leaves name_template unset — should inherit from defaults
            ..Default::default()
        }]);

        apply_to_crate(&defaults, &mut crate_cfg);

        if let ArchivesConfig::Configs(list) = &crate_cfg.archives {
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].name_template, Some("DEFAULT".to_string()));
        } else {
            panic!("expected Configs variant");
        }
    }

    // --------------- (4) {} = inherit-all ---------------

    #[test]
    fn empty_per_crate_block_inherits_all_from_defaults() {
        // checksum: {} at per-crate position means "inherit everything".
        let defaults = Defaults {
            checksum: Some(ChecksumConfig {
                algorithm: Some("sha512".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.checksum = Some(ChecksumConfig::default()); // empty `{}`

        apply_to_crate(&defaults, &mut crate_cfg);

        let checksum = crate_cfg.checksum.unwrap();
        assert_eq!(checksum.algorithm, Some("sha512".to_string()));
    }

    // --------------- (5) skip: true = suppress block ---------------

    #[test]
    fn per_crate_skip_true_suppresses_inherited_block() {
        // DEC-9: `skip: true` at per-crate position suppresses the
        // inherited block entirely — the merge engine must not fill any
        // field from defaults when the per-crate value carries skip:true.
        let defaults = Defaults {
            checksum: Some(ChecksumConfig {
                algorithm: Some("sha512".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.checksum = Some(ChecksumConfig {
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        });

        apply_to_crate(&defaults, &mut crate_cfg);

        let checksum = crate_cfg.checksum.unwrap();
        assert_eq!(checksum.skip, Some(StringOrBool::Bool(true)));
        // algorithm stays None — defaults were suppressed by skip:true.
        assert_eq!(checksum.algorithm, None);
    }

    // --------------- (6) Per-crate scalar wins ---------------

    #[test]
    fn per_crate_scalar_wins_over_defaults() {
        let defaults = Defaults {
            cross: Some(CrossStrategy::Auto),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.cross = Some(CrossStrategy::Zigbuild);

        apply_to_crate(&defaults, &mut crate_cfg);

        assert_eq!(crate_cfg.cross, Some(CrossStrategy::Zigbuild));
    }

    #[test]
    fn defaults_scalar_fills_when_crate_unset() {
        let defaults = Defaults {
            cross: Some(CrossStrategy::Cross),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        // crate.cross is None

        apply_to_crate(&defaults, &mut crate_cfg);

        assert_eq!(crate_cfg.cross, Some(CrossStrategy::Cross));
    }

    // --------------- Apply-defaults entry point: idempotent ---------------

    #[test]
    fn apply_defaults_is_idempotent() {
        let mut config = Config {
            crates: vec![make_crate("a")],
            defaults: Some(Defaults {
                cross: Some(CrossStrategy::Auto),
                archives: Some(ArchiveConfig {
                    format: Some("tar.gz".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        apply_defaults(&mut config);
        let after_first = config.clone();
        apply_defaults(&mut config);

        // Second invocation must produce identical state.
        assert_eq!(config.crates[0].cross, after_first.crates[0].cross);
        if let (ArchivesConfig::Configs(a), ArchivesConfig::Configs(b)) =
            (&config.crates[0].archives, &after_first.crates[0].archives)
        {
            assert_eq!(a.len(), b.len());
        }
    }

    // --------------- Builds template fold ---------------

    #[test]
    fn defaults_builds_fills_per_build_settings_when_crate_unset() {
        use crate::config::BuildConfig;
        let defaults = Defaults {
            builds: Some(BuildConfig {
                binary: String::new(),
                flags: Some("--release --locked".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.builds = Some(vec![BuildConfig {
            binary: "myapp".to_string(),
            ..Default::default()
        }]);

        apply_to_crate(&defaults, &mut crate_cfg);

        let builds = crate_cfg.builds.unwrap();
        assert_eq!(builds.len(), 1);
        assert_eq!(builds[0].binary, "myapp", "crate field should win");
        assert_eq!(builds[0].flags, Some("--release --locked".to_string()));
    }
}
