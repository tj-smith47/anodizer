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
// Skip-block suppression (DEC-9)
// ---------------------------------------------------------------------------
//
// Per DEC-9, any per-crate config block carrying `skip: true` (a `StringOrBool`
// that evaluates to a truthy value) suppresses inheritance from the matching
// `defaults.*` block entirely — the merge engine treats defaults as if they
// were `None` for that block.
//
// Rather than adding a `is_skipped` accessor on every config struct that
// happens to carry a `skip` field (24+ types and growing), we inspect the
// serialized JSON form of the value and look for a `skip` key whose value is
// truthy. This keeps the suppression rule uniform across every block — adding
// a new `skip`-bearing config type requires no changes here.

/// Returns `true` when the per-crate value's serialized JSON form has a
/// `skip` field whose value evaluates to truthy (`true`, `"true"`, `"1"`).
/// Returns `false` for any other shape — including values without a `skip`
/// field, falsey skip values, and serialization failures (defaults inherit
/// in those cases, matching prior behaviour).
fn is_skipped<T: Serialize>(value: &T) -> bool {
    let Ok(json) = serde_json::to_value(value) else {
        return false;
    };
    json_skip_truthy(&json)
}

/// Returns `true` when `value` is an object containing a `skip` key whose
/// value is a truthy bool / truthy string ("true" / "1").
fn json_skip_truthy(value: &Json) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    let Some(skip) = obj.get("skip") else {
        return false;
    };
    match skip {
        Json::Bool(b) => *b,
        Json::String(s) => matches!(s.trim(), "true" | "1"),
        _ => false,
    }
}

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
    // Per DEC-9, the per-crate block setting `skip: true` suppresses
    // inheritance entirely — handled inside `deep_merge_option` via the
    // generic `is_skipped` JSON inspector so every `skip`-bearing block
    // (24+ today: checksum, source, upx, sign, notarize, sbom, snapcraft,
    // dmg, msi, pkg, nsis, app_bundle, flatpak, docker_v2, ...) gets the
    // same suppression behaviour.
    deep_merge_option(&mut crate_cfg.checksum, defaults.checksum.as_ref());

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
///
/// **Skip-suppression (DEC-9):** when the per-crate value's serialised form
/// has a truthy `skip` field, defaults are not merged in — the user's intent
/// is to disable the block, so inheriting fields would be wrong.
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
            if is_skipped(crate_val) {
                return;
            }
            deep_merge_struct_inplace(crate_val, defaults_val);
        }
    }
}

/// Deep-merge `defaults` into `target` so any field the target left as
/// `null` (i.e. `None` on the original Option) is filled from defaults.
/// Other fields are left untouched.
///
/// Defaults merging is best-effort by design — a serialise / deserialise
/// failure here leaves `target` unchanged rather than failing the whole
/// pipeline. We still surface the failure on stderr so that genuinely broken
/// configs surface in CI rather than silently dropping defaults.
fn deep_merge_struct_inplace<T: Serialize + DeserializeOwned>(target: &mut T, defaults: &T) {
    let type_name = std::any::type_name::<T>();
    let mut crate_json = match serde_json::to_value(&*target) {
        Ok(v) => v,
        Err(err) => {
            eprintln!(
                "[defaults_merge] WARNING: failed to serialize target of type {type_name}: \
                 {err}; defaults inheritance skipped for this field"
            );
            return;
        }
    };
    let defaults_json = match serde_json::to_value(defaults) {
        Ok(v) => v,
        Err(err) => {
            eprintln!(
                "[defaults_merge] WARNING: failed to serialize defaults of type {type_name}: \
                 {err}; defaults inheritance skipped for this field"
            );
            return;
        }
    };
    deep_merge_json(&mut crate_json, &defaults_json);
    match serde_json::from_value::<T>(crate_json) {
        Ok(merged) => *target = merged,
        Err(err) => {
            eprintln!(
                "[defaults_merge] WARNING: failed to deserialize merged value of type \
                 {type_name}: {err}; defaults inheritance skipped for this field"
            );
        }
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

fn archive_identity(a: &ArchiveConfig) -> Option<String> {
    a.format.clone()
}

// ---------------------------------------------------------------------------
// List merge: generic "single defaults entry → Vec<T> per-crate" path
// ---------------------------------------------------------------------------

fn merge_list_by_identity<T, F>(target: &mut Option<Vec<T>>, defaults: Option<&T>, identity: F)
where
    T: Clone + Serialize + DeserializeOwned,
    F: Fn(&T) -> Option<String>,
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

/// Core "merge one defaults entry into a list" routine.
///
/// Behaviour (DEC-9):
/// - **Empty list**: append the defaults entry.
/// - **Identity match (`Some(x) == Some(x)`)**: deep-merge defaults into the
///   FIRST matching per-crate entry only; subsequent matches are left
///   untouched. This avoids fanning a single defaults entry into N existing
///   entries that happen to share the same identity (e.g. through a
///   typo-duplicated key).
/// - **No identity match**: append the defaults entry after the per-crate
///   entries.
/// - **`None` identity**: never matches another `None` (two unkeyed entries
///   stay distinct). Required to keep unrelated unkeyed entries from
///   collapsing into the same defaults block.
/// - **Skip-suppression**: any per-crate entry whose serialised form has a
///   truthy `skip` field is skipped over for matching purposes — the
///   user's intent is to disable that entry, not have it absorb defaults.
fn merge_one_into_list<T, F>(list: &mut Vec<T>, default_entry: &T, identity: F)
where
    T: Clone + Serialize + DeserializeOwned,
    F: Fn(&T) -> Option<String>,
{
    if list.is_empty() {
        list.push(default_entry.clone());
        return;
    }
    let default_id = identity(default_entry);
    let mut handled = false;
    for entry in list.iter_mut() {
        if !identity_matches(&identity(entry), &default_id) {
            continue;
        }
        // Identity match. Two cases:
        // 1. Per-crate entry has `skip: true` → suppress inheritance entirely
        //    (do not merge, do not append a duplicate).
        // 2. Otherwise → deep-merge defaults into this entry.
        if !is_skipped(entry) {
            deep_merge_struct_inplace(entry, default_entry);
        }
        handled = true;
        // Stop after the first match — defaults are a single entry and
        // must not fan out into multiple per-crate entries even when
        // several share the same identity key.
        break;
    }
    if !handled {
        list.push(default_entry.clone());
    }
}

/// Identity match: two `Some(x)` with equal payloads match; everything else
/// (including `None == None`) does not. Keeps unkeyed entries distinct.
fn identity_matches(a: &Option<String>, b: &Option<String>) -> bool {
    matches!((a, b), (Some(x), Some(y)) if x == y)
}

// ---------------------------------------------------------------------------
// Identity functions per packaging type
// ---------------------------------------------------------------------------

fn nfpm_identity(c: &NfpmConfig) -> Option<String> {
    // GoReleaser identity: id → package_name → none (unkeyed).
    if let Some(ref id) = c.id {
        return Some(id.clone());
    }
    if let Some(ref pkg) = c.package_name {
        return Some(pkg.clone());
    }
    None
}

fn snapcraft_identity(c: &SnapcraftConfig) -> Option<String> {
    c.name.clone()
}

fn dmg_identity(c: &DmgConfig) -> Option<String> {
    c.id.clone()
}

fn pkg_identity(c: &PkgConfig) -> Option<String> {
    c.id.clone()
}

fn msi_identity(c: &MsiConfig) -> Option<String> {
    c.id.clone()
}

fn nsis_identity(c: &NsisConfig) -> Option<String> {
    c.id.clone()
}

fn app_bundle_identity(c: &AppBundleConfig) -> Option<String> {
    c.id.clone()
}

fn flatpak_identity(c: &FlatpakConfig) -> Option<String> {
    c.id.clone()
}

fn docker_v2_identity(c: &DockerV2Config) -> Option<String> {
    c.id.clone()
}

// ---------------------------------------------------------------------------
// Publisher defaults
// ---------------------------------------------------------------------------

fn merge_publish_defaults(target: &mut PublishConfig, defaults: &PublishDefaults) {
    deep_merge_option(&mut target.homebrew, defaults.homebrew.as_ref());
    deep_merge_option(&mut target.cargo, defaults.cargo.as_ref());
    deep_merge_option(&mut target.scoop, defaults.scoop.as_ref());
    deep_merge_option(&mut target.winget, defaults.winget.as_ref());
    deep_merge_option(&mut target.chocolatey, defaults.chocolatey.as_ref());
    deep_merge_option(&mut target.krew, defaults.krew.as_ref());
    deep_merge_option(&mut target.nix, defaults.nix.as_ref());
    deep_merge_option(&mut target.aur, defaults.aur.as_ref());
    deep_merge_option(&mut target.aur_source, defaults.aur_source.as_ref());
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

    // --------------- Cargo (crates.io) publisher defaults ---------------

    #[test]
    fn cargo_defaults_merge_into_per_crate_publish_cargo_when_unset() {
        use crate::config::CargoPublishConfig;

        let defaults = Defaults {
            publish: Some(PublishDefaults {
                cargo: Some(CargoPublishConfig {
                    index_timeout: Some(600),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        // Per-crate has no publish block at all.
        let mut crate_cfg = make_crate("mycrate");

        apply_to_crate(&defaults, &mut crate_cfg);

        let publish = crate_cfg.publish.expect("publish block should be created");
        let cargo = publish
            .cargo
            .expect("publish.cargo should be inherited from defaults");
        assert_eq!(
            cargo.index_timeout,
            Some(600),
            "expected index_timeout=600 inherited from defaults.cargo"
        );
    }

    #[test]
    fn cargo_defaults_fill_missing_fields_but_per_crate_wins_on_collision() {
        use crate::config::CargoPublishConfig;

        let defaults = Defaults {
            publish: Some(PublishDefaults {
                cargo: Some(CargoPublishConfig {
                    index_timeout: Some(600),
                    no_verify: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        // Per-crate explicitly sets index_timeout=120 and no other fields.
        let mut crate_cfg = make_crate("mycrate");
        crate_cfg.publish = Some(PublishConfig {
            cargo: Some(CargoPublishConfig {
                index_timeout: Some(120),
                ..Default::default()
            }),
            ..Default::default()
        });

        apply_to_crate(&defaults, &mut crate_cfg);

        let publish = crate_cfg.publish.unwrap();
        let cargo = publish.cargo.unwrap();
        assert_eq!(
            cargo.index_timeout,
            Some(120),
            "per-crate index_timeout should win over defaults"
        );
        assert_eq!(
            cargo.no_verify,
            Some(true),
            "no_verify should be filled from defaults (per-crate left it unset)"
        );
    }

    // --------------- Builds template fold ---------------

    #[test]
    fn defaults_builds_fills_per_build_settings_when_crate_unset() {
        use crate::config::BuildConfig;
        let defaults = Defaults {
            builds: Some(BuildConfig {
                // defaults.builds has no `binary` — that's the whole point
                // of path-mirror inheritance: per-crate supplies the binary.
                binary: None,
                flags: Some("--release --locked".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.builds = Some(vec![BuildConfig {
            binary: Some("myapp".to_string()),
            ..Default::default()
        }]);

        apply_to_crate(&defaults, &mut crate_cfg);

        let builds = crate_cfg.builds.unwrap();
        assert_eq!(builds.len(), 1);
        assert_eq!(
            builds[0].binary,
            Some("myapp".to_string()),
            "crate field should win"
        );
        assert_eq!(builds[0].flags, Some("--release --locked".to_string()));
    }

    // --------------- (I-5) Workspace-overlay path ---------------

    #[test]
    fn defaults_apply_to_workspace_crates() {
        // `apply_defaults` must fold defaults into every crate inside
        // `config.workspaces[].crates`, not just top-level `config.crates`.
        // The CLI later replaces `config.crates` with the active workspace's
        // crates via `apply_workspace_overlay`, so without this branch the
        // workspace-axis pipeline would lose all defaults inheritance.
        use crate::config::WorkspaceConfig;

        let mut config = Config {
            workspaces: Some(vec![WorkspaceConfig {
                name: "ws1".to_string(),
                crates: vec![make_crate("a")],
                ..Default::default()
            }]),
            defaults: Some(Defaults {
                cross: Some(CrossStrategy::Auto),
                ..Default::default()
            }),
            ..Default::default()
        };

        apply_defaults(&mut config);

        let workspaces = config.workspaces.expect("workspaces should remain Some");
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].crates.len(), 1);
        assert_eq!(
            workspaces[0].crates[0].cross,
            Some(CrossStrategy::Auto),
            "defaults.cross should fold into workspace crates"
        );
    }

    // --------------- (I-1) Generic skip suppression ---------------

    #[test]
    fn per_crate_skip_true_suppresses_arbitrary_block() {
        // DEC-9 applies broadly: any block with `skip: true` blocks
        // inheritance. Verify on a non-checksum block to prove the skip
        // suppression is generic, not tied to a specific config type.
        use crate::config::SnapcraftConfig;
        let defaults = Defaults {
            snapcrafts: Some(SnapcraftConfig {
                name: Some("mysnap".to_string()),
                summary: Some("DEFAULT-SUMMARY".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.snapcrafts = Some(vec![SnapcraftConfig {
            name: Some("mysnap".to_string()),
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }]);

        apply_to_crate(&defaults, &mut crate_cfg);

        let snaps = crate_cfg.snapcrafts.unwrap();
        assert_eq!(snaps.len(), 1, "skip:true should not append a duplicate");
        assert_eq!(
            snaps[0].summary, None,
            "skip:true must suppress defaults.summary inheritance"
        );
    }

    // --------------- (M-6) Single defaults entry, fan-out guard ---------------

    #[test]
    fn defaults_entry_does_not_fan_out_into_multiple_matching_entries() {
        // Two unkeyed archives that happen to share an empty identity must
        // NOT both absorb the same defaults entry. After the M-7 fix to
        // identity functions returning Option<String>, neither matches the
        // defaults (whose format is "tar.gz") so defaults are appended;
        // re-run with matching identity to confirm only the first absorbs.
        let defaults = Defaults {
            archives: Some(ArchiveConfig {
                format: Some("tar.gz".to_string()),
                name_template: Some("DEFAULT".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.archives = ArchivesConfig::Configs(vec![
            ArchiveConfig {
                format: Some("tar.gz".to_string()),
                ..Default::default()
            },
            ArchiveConfig {
                format: Some("tar.gz".to_string()),
                name_template: Some("EXISTING".to_string()),
                ..Default::default()
            },
        ]);

        apply_to_crate(&defaults, &mut crate_cfg);

        if let ArchivesConfig::Configs(list) = &crate_cfg.archives {
            assert_eq!(list.len(), 2, "should not append a third entry");
            // First matching entry inherits the defaults name_template.
            assert_eq!(list[0].name_template, Some("DEFAULT".to_string()));
            // Second matching entry MUST stay untouched (no fan-out).
            assert_eq!(list[1].name_template, Some("EXISTING".to_string()));
        } else {
            panic!("expected Configs variant");
        }
    }

    // --------------- (M-7) Unkeyed entries stay distinct ---------------

    #[test]
    fn unkeyed_entries_stay_distinct_no_collapse() {
        // Two archives with no `format` set both have identity `None`.
        // They MUST NOT collapse into each other when defaults arrive —
        // the defaults entry should append once, not merge into either.
        let defaults = Defaults {
            archives: Some(ArchiveConfig {
                format: Some("tar.gz".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.archives = ArchivesConfig::Configs(vec![
            ArchiveConfig {
                format: None,
                name_template: Some("FIRST".to_string()),
                ..Default::default()
            },
            ArchiveConfig {
                format: None,
                name_template: Some("SECOND".to_string()),
                ..Default::default()
            },
        ]);

        apply_to_crate(&defaults, &mut crate_cfg);

        if let ArchivesConfig::Configs(list) = &crate_cfg.archives {
            // Two unkeyed entries + one defaults entry (different identity) = 3.
            assert_eq!(list.len(), 3, "unkeyed entries must stay distinct");
            assert_eq!(list[0].name_template, Some("FIRST".to_string()));
            assert_eq!(list[1].name_template, Some("SECOND".to_string()));
            assert_eq!(list[2].format, Some("tar.gz".to_string()));
        } else {
            panic!("expected Configs variant");
        }
    }
}
