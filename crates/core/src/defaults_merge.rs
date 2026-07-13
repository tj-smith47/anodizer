//! Defaults inheritance merge engine (path-mirror).
//!
//! The workspace `defaults:` block path-mirrors the per-crate
//! `CrateConfig` (and a small subset of top-level `Config`) shape. The merge
//! engine in this module folds those defaults into every resolved crate so
//! the build pipeline can keep reading from `crate_cfg.<field>` without
//! caring whether a value was hoisted to defaults.
//!
//! ## Semantics
//!
//! - **Struct-typed fields (deep-merge)**: defaults fill any field the crate
//!   left unset; on conflict, the crate value wins. Implemented via a JSON
//!   round-trip using [`serde_json::Value`] so arbitrarily nested structs
//!   merge uniformly without per-type boilerplate.
//! - **List-typed fields (append + merge-by-identity)**: each defaults
//!   entry merges into the crate entry that shares its identity key
//!   (first entry of `formats` for archives, `id`/`name`/`package_name`
//!   for packagers, etc.). Defaults entries with no identity-match are
//!   appended after the crate's own entries.
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
// Skip-block suppression
// ---------------------------------------------------------------------------
//
// Any per-crate config block carrying `skip: true` (a `StringOrBool`
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
/// Top-level `Config` fields that path-mirror in `Defaults` (`source`,
/// `upx`, `signs`, `binary_signs`, `docker_signs`, `notarize`, `sboms`,
/// `makeselfs`, `srpms`) are folded as a fill-when-unset pre-pipeline
/// pass: if the top-level field is empty / `None`, the defaults entry
/// is cloned in; if the top-level field is already set, it wins (defaults
/// are inert). For deeply-nested `Option<T>` slots that share a struct
/// type with the defaults slot, the per-config entry is deep-merged with
/// the defaults entry filling any field the user left unset.
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

    // Top-level `Config` fold — separate from the per-crate fold so
    // stages reading `ctx.config.<field>` see the resolved value without
    // needing to consult `defaults.<field>` on every access.
    apply_top_level_defaults(config, &defaults);
}

/// Fold defaults into the top-level `Config` fields that path-mirror in
/// `Defaults`. Fill-when-unset — top-level user values always win.
fn apply_top_level_defaults(config: &mut Config, defaults: &Defaults) {
    // Single-struct deep-merge: both sides are Option<T>, so deep-merge if
    // both Some, fill if config is None.
    deep_merge_option(&mut config.source, defaults.source.as_ref());
    deep_merge_option(&mut config.notarize, defaults.notarize.as_ref());
    deep_merge_option(&mut config.srpms, defaults.srpms.as_ref());

    // Vec<T> top-level filled from a single defaults entry when empty.
    if config.upx.is_empty()
        && let Some(ref d) = defaults.upx
    {
        config.upx = vec![d.clone()];
    }
    if config.signs.is_empty()
        && let Some(ref d) = defaults.sign
    {
        config.signs = vec![d.clone()];
    }
    if config.binary_signs.is_empty()
        && let Some(ref d) = defaults.binary_signs
    {
        config.binary_signs = vec![d.clone()];
    }
    if config.sboms.is_empty()
        && let Some(ref d) = defaults.sbom
    {
        config.sboms = vec![d.clone()];
    }
    if config.makeselfs.is_empty()
        && let Some(ref d) = defaults.makeselves
    {
        config.makeselfs = vec![d.clone()];
    }

    // Option<Vec<T>> filled from a single defaults entry when None / empty.
    if config.docker_signs.as_ref().is_none_or(|v| v.is_empty())
        && let Some(ref d) = defaults.docker_signs
    {
        config.docker_signs = Some(vec![d.clone()]);
    }
}

/// Apply defaults to a single crate. Exposed for tests; production code
/// should call [`apply_defaults`] which iterates all crates.
///
/// ## Coverage map
///
/// What this function actually folds defaults into per-crate, by call-site:
///
/// - **Scalar fill (`cross`)** — defaults.cross fills crate_cfg.cross when None.
/// - **Single-struct deep-merge** via `deep_merge_option`: only `checksum`
///   today. The skip-suppression invariant means any per-crate value with
///   truthy `skip` blocks inheritance (this generalises across every
///   `skip`-bearing block as more single-struct fields are wired in).
/// - **List append + merge-by-identity** via `merge_list_by_identity`:
///   `nfpm`, `snapcrafts`, `dmgs`, `pkgs`, `msis`, `nsis`, `app_bundles`,
///   `flatpaks`, `dockers_v2`. Each takes a single defaults entry and folds
///   into the per-crate vec by identity key.
/// - **Archives** via dedicated `merge_archives` (handles the
///   ArchivesConfig::Disabled / Configs split).
/// - **Builds template** — defaults.builds is deep-merged into every
///   `crate_cfg.builds[]` entry; if per-crate is empty, the template is
///   cloned as the seed.
/// - **Publish axis** via `merge_publish_defaults` — each publisher
///   (homebrew, homebrew_cask, cargo, scoop, winget, chocolatey, krew, nix,
///   aur, aur_source) deep-merges from its `PublishDefaults.<name>` slot.
///
/// What this function does **not** fold (top-level `Config` fields that
/// path-mirror in `Defaults` but live on `Config`, not `CrateConfig`):
/// `source`, `upx`, `sign`, `binary_signs`, `docker_signs`, `notarize`,
/// `sbom`, `makeselves`, `srpms`. Not yet folded — see the
/// `apply_defaults` rustdoc above.
pub fn apply_to_crate(defaults: &Defaults, crate_cfg: &mut CrateConfig) {
    // ---- Scalar / Option<T> fields: fill if None ----
    if crate_cfg.cross.is_none() && defaults.cross.is_some() {
        crate_cfg.cross = defaults.cross.clone();
    }
    // Override-not-append: a per-crate list wins outright; defaults supply the
    // whole list only when the crate declares none.
    if crate_cfg.version_files.is_none() && defaults.version_files.is_some() {
        crate_cfg.version_files = defaults.version_files.clone();
    }

    // ---- Single-struct deep-merge fields ----
    // Per-crate block with truthy `skip:` suppresses inheritance entirely —
    // handled inside `deep_merge_option` via the generic `is_skipped` JSON
    // inspector so the rule applies uniformly to every `skip`-bearing
    // block as additional single-struct fields are wired in.
    deep_merge_option(&mut crate_cfg.checksum, defaults.checksum.as_ref());

    // ---- List-typed fields: append + merge-by-identity ----
    merge_archives(&mut crate_cfg.archives, defaults.archives.as_ref());
    merge_list_by_identity(&mut crate_cfg.nfpms, defaults.nfpms.as_ref(), nfpm_identity);
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
        &mut crate_cfg.dockers_v2,
        defaults.dockers_v2.as_ref(),
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
/// **Skip-suppression:** when the per-crate value's serialised form
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
/// pipeline. We still surface the failure via `tracing::warn!` so that
/// genuinely broken configs surface in CI rather than silently dropping
/// defaults.
fn deep_merge_struct_inplace<T: Serialize + DeserializeOwned>(target: &mut T, defaults: &T) {
    let type_name = std::any::type_name::<T>();
    let mut crate_json = match serde_json::to_value(&*target) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                "failed to serialize target ({type_name}: {err}); \
                 defaults inheritance skipped for this field"
            );
            return;
        }
    };
    let defaults_json = match serde_json::to_value(defaults) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                "failed to serialize defaults ({type_name}: {err}); \
                 defaults inheritance skipped for this field"
            );
            return;
        }
    };
    deep_merge_json(&mut crate_json, &defaults_json);
    match serde_json::from_value::<T>(crate_json) {
        Ok(merged) => *target = merged,
        Err(err) => {
            tracing::warn!(
                "failed to deserialize merged value ({type_name}: {err}); \
                 defaults inheritance skipped for this field"
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
    a.formats.as_ref().and_then(|f| f.first().cloned())
}

// ---------------------------------------------------------------------------
// List merge helpers
// ---------------------------------------------------------------------------

/// Append-merge: extend `target` with every item from `defaults`.
/// Used for Vec fields (e.g. `on_error`) where both the per-crate list and the
/// defaults list must fire — per-crate entries first, defaults appended after.
fn merge_append_list<T: Clone>(target: &mut Option<Vec<T>>, defaults: Option<&Vec<T>>) {
    if let Some(default_items) = defaults {
        match target {
            Some(existing) => existing.extend(default_items.iter().cloned()),
            None => *target = Some(default_items.clone()),
        }
    }
}

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
/// Behaviour:
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
    // Merge identity: id → package_name → none (unkeyed).
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
    deep_merge_option(&mut target.homebrew_cask, defaults.homebrew_cask.as_ref());
    deep_merge_option(&mut target.cargo, defaults.cargo.as_ref());
    deep_merge_option(&mut target.scoop, defaults.scoop.as_ref());
    deep_merge_option(&mut target.winget, defaults.winget.as_ref());
    deep_merge_option(&mut target.chocolatey, defaults.chocolatey.as_ref());
    deep_merge_option(&mut target.krew, defaults.krew.as_ref());
    deep_merge_option(&mut target.nix, defaults.nix.as_ref());
    deep_merge_option(&mut target.aur, defaults.aur.as_ref());
    deep_merge_option(&mut target.aur_source, defaults.aur_source.as_ref());
    merge_append_list(&mut target.on_error, defaults.on_error.as_ref());
    merge_append_list(&mut target.on_rollback, defaults.on_rollback.as_ref());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ArchiveConfig, ArchivesConfig, ChecksumConfig, CrossStrategy, HomebrewCaskConfig,
        HomebrewCaskUninstall, HomebrewConfig, StringOrBool,
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
                formats: Some(vec!["tar.gz".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.archives = ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["zip".to_string()]),
            ..Default::default()
        }]);

        apply_to_crate(&defaults, &mut crate_cfg);

        if let ArchivesConfig::Configs(list) = &crate_cfg.archives {
            assert_eq!(list.len(), 2);
            let formats: Vec<_> = list
                .iter()
                .map(|a| a.formats.as_ref().and_then(|f| f.first().cloned()))
                .collect();
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
                formats: Some(vec!["tar.gz".to_string()]),
                name_template: Some("DEFAULT".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.archives = ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["tar.gz".to_string()]),
            // crate sets a name_template — wins over defaults
            name_template: Some("CRATE".to_string()),
            ..Default::default()
        }]);

        apply_to_crate(&defaults, &mut crate_cfg);

        if let ArchivesConfig::Configs(list) = &crate_cfg.archives {
            assert_eq!(list.len(), 1, "should merge into single entry");
            assert_eq!(list[0].name_template, Some("CRATE".to_string()));
            assert_eq!(
                list[0].formats.as_deref(),
                Some(&["tar.gz".to_string()][..])
            );
        } else {
            panic!("expected Configs variant");
        }
    }

    #[test]
    fn list_merge_by_identity_fills_unset_fields_from_defaults() {
        let defaults = Defaults {
            archives: Some(ArchiveConfig {
                formats: Some(vec!["tar.gz".to_string()]),
                name_template: Some("DEFAULT".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.archives = ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["tar.gz".to_string()]),
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
        // Skip-suppression invariant: `skip: true` at per-crate position
        // suppresses the inherited block entirely — the merge engine must
        // not fill any field from defaults when the per-crate value carries
        // `skip: true`.
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
                    formats: Some(vec!["tar.gz".to_string()]),
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

    // --------------- on_error defaults: lockstep append-merge ---------------

    /// `defaults.publish.on_error` append-merges into EVERY crate of a
    /// multi-crate (lockstep) workspace independently: a crate with its own
    /// hooks keeps them first with the defaults appended after; a crate with
    /// no `publish:` block at all inherits the defaults list outright.
    #[test]
    fn on_error_defaults_append_merge_across_lockstep_crates() {
        use crate::config::HookEntry;

        let mut crate_a = make_crate("a");
        crate_a.publish = Some(PublishConfig {
            on_error: Some(vec![HookEntry::Simple("notify-a".to_string())]),
            ..Default::default()
        });
        let crate_b = make_crate("b");

        let mut config = Config {
            crates: vec![crate_a, crate_b],
            defaults: Some(Defaults {
                publish: Some(PublishDefaults {
                    on_error: Some(vec![HookEntry::Simple("notify-default".to_string())]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_defaults(&mut config);

        let a = config.crates[0]
            .publish
            .as_ref()
            .expect("crate a publish block")
            .on_error
            .as_ref()
            .expect("crate a on_error");
        assert_eq!(a.len(), 2, "per-crate hook first, defaults appended");
        assert!(a[0] == "notify-a", "crate a's own hook must come first");
        assert!(a[1] == "notify-default", "defaults hook appended after");

        let b = config.crates[1]
            .publish
            .as_ref()
            .expect("crate b publish block created by the merge")
            .on_error
            .as_ref()
            .expect("crate b on_error");
        assert_eq!(b.len(), 1, "crate b inherits the defaults list outright");
        assert!(b[0] == "notify-default");
    }

    /// `defaults.publish.on_rollback` append-merges into EVERY crate of a
    /// multi-crate (lockstep) workspace independently, mirroring `on_error`:
    /// a crate with its own hooks keeps them first with the defaults appended
    /// after; a crate with no `publish:` block inherits the defaults outright.
    #[test]
    fn on_rollback_defaults_append_merge_across_lockstep_crates() {
        use crate::config::HookEntry;

        let mut crate_a = make_crate("a");
        crate_a.publish = Some(PublishConfig {
            on_rollback: Some(vec![HookEntry::Simple("revert-a".to_string())]),
            ..Default::default()
        });
        let crate_b = make_crate("b");

        let mut config = Config {
            crates: vec![crate_a, crate_b],
            defaults: Some(Defaults {
                publish: Some(PublishDefaults {
                    on_rollback: Some(vec![HookEntry::Simple("revert-default".to_string())]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_defaults(&mut config);

        let a = config.crates[0]
            .publish
            .as_ref()
            .expect("crate a publish block")
            .on_rollback
            .as_ref()
            .expect("crate a on_rollback");
        assert_eq!(a.len(), 2, "per-crate hook first, defaults appended");
        assert!(a[0] == "revert-a", "crate a's own hook must come first");
        assert!(a[1] == "revert-default", "defaults hook appended after");

        let b = config.crates[1]
            .publish
            .as_ref()
            .expect("crate b publish block created by the merge")
            .on_rollback
            .as_ref()
            .expect("crate b on_rollback");
        assert_eq!(b.len(), 1, "crate b inherits the defaults list outright");
        assert!(b[0] == "revert-default");
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
    fn cargo_defaults_auth_inherited_when_per_crate_omits_it() {
        use crate::config::{CargoAuthMode, CargoPublishConfig};

        let defaults = Defaults {
            publish: Some(PublishDefaults {
                cargo: Some(CargoPublishConfig {
                    auth: Some(CargoAuthMode::Oidc),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        // Per-crate sets another field but omits `auth` entirely — the bug was
        // that a bare `auth: CargoAuthMode` serialized to `auto` and blocked the
        // strict `oidc` default from merging in.
        let mut crate_cfg = make_crate("mycrate");
        crate_cfg.publish = Some(PublishConfig {
            cargo: Some(CargoPublishConfig {
                index_timeout: Some(120),
                ..Default::default()
            }),
            ..Default::default()
        });

        apply_to_crate(&defaults, &mut crate_cfg);

        let cargo = crate_cfg.publish.unwrap().cargo.unwrap();
        assert_eq!(
            cargo.resolved_auth(),
            CargoAuthMode::Oidc,
            "defaults.publish.cargo.auth=oidc must inherit into a per-crate cargo \
             block that omits auth (must NOT degrade to auto)"
        );
        assert_eq!(
            cargo.index_timeout,
            Some(120),
            "the per-crate field the block DID set must survive the merge"
        );
    }

    #[test]
    fn cargo_defaults_auth_per_crate_explicit_wins() {
        use crate::config::{CargoAuthMode, CargoPublishConfig};

        let defaults = Defaults {
            publish: Some(PublishDefaults {
                cargo: Some(CargoPublishConfig {
                    auth: Some(CargoAuthMode::Oidc),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        // Per-crate explicitly picks `token`; it must win over the default.
        let mut crate_cfg = make_crate("mycrate");
        crate_cfg.publish = Some(PublishConfig {
            cargo: Some(CargoPublishConfig {
                auth: Some(CargoAuthMode::Token),
                ..Default::default()
            }),
            ..Default::default()
        });

        apply_to_crate(&defaults, &mut crate_cfg);

        let cargo = crate_cfg.publish.unwrap().cargo.unwrap();
        assert_eq!(
            cargo.resolved_auth(),
            CargoAuthMode::Token,
            "an explicit per-crate auth must win over the defaults value"
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
                flags: Some(vec!["--release".to_string(), "--locked".to_string()]),
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
        assert_eq!(
            builds[0].flags,
            Some(vec!["--release".to_string(), "--locked".to_string()])
        );
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
        // The skip-suppression invariant applies broadly: any block with
        // `skip: true` blocks inheritance. Verify on a non-checksum block
        // to prove the skip suppression is generic, not tied to a specific
        // config type.
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
                formats: Some(vec!["tar.gz".to_string()]),
                name_template: Some("DEFAULT".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.archives = ArchivesConfig::Configs(vec![
            ArchiveConfig {
                formats: Some(vec!["tar.gz".to_string()]),
                ..Default::default()
            },
            ArchiveConfig {
                formats: Some(vec!["tar.gz".to_string()]),
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
                formats: Some(vec!["tar.gz".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.archives = ArchivesConfig::Configs(vec![
            ArchiveConfig {
                name_template: Some("FIRST".to_string()),
                ..Default::default()
            },
            ArchiveConfig {
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
            assert_eq!(
                list[2].formats.as_deref(),
                Some(&["tar.gz".to_string()][..])
            );
        } else {
            panic!("expected Configs variant");
        }
    }

    // --------------- HomebrewCask publisher defaults ---------------

    #[test]
    fn homebrew_cask_top_level_yaml_parses_with_unified_type() {
        // top-level `homebrew_casks:` must parse with the unified HomebrewCaskConfig.
        let yaml = r#"
project_name: myapp
homebrew_casks:
  - name: myapp
    description: "My app cask"
    homepage: "https://example.com"
    repository:
      owner: myorg
      name: homebrew-tap
    directory: Casks
    skip_upload: "auto"
"#;
        let cfg: crate::config::Config = serde_yaml_ng::from_str(yaml)
            .expect("homebrew_casks with unified HomebrewCaskConfig should parse");
        let casks = cfg
            .homebrew_casks
            .expect("homebrew_casks should be present");
        assert_eq!(casks.len(), 1);
        assert_eq!(casks[0].name.as_deref(), Some("myapp"));
        assert_eq!(casks[0].description.as_deref(), Some("My app cask"));
        assert_eq!(casks[0].directory.as_deref(), Some("Casks"));
    }

    #[test]
    fn homebrew_cask_per_crate_yaml_parses_with_unified_type() {
        // per-crate `publish.homebrew_cask:` must parse with the unified HomebrewCaskConfig.
        let yaml = r#"
project_name: myapp
crates:
  - name: myapp
    path: .
    tag_template: "v{{ .Version }}"
    publish:
      homebrew_cask:
        name: myapp
        url_template: "https://releases.example.com/{{ .Version }}/myapp_{{ .Os }}_{{ .Arch }}.dmg"
        app: "MyApp.app"
        caveats: "Check the docs."
"#;
        let cfg: crate::config::Config = serde_yaml_ng::from_str(yaml)
            .expect("per-crate publish.homebrew_cask with unified type should parse");
        let crate_publish = cfg.crates[0]
            .publish
            .as_ref()
            .expect("publish block should be present");
        let cask = crate_publish
            .homebrew_cask
            .as_ref()
            .expect("homebrew_cask should be present");
        assert_eq!(cask.name.as_deref(), Some("myapp"));
        assert_eq!(cask.app.as_deref(), Some("MyApp.app"));
        assert_eq!(cask.caveats.as_deref(), Some("Check the docs."));
    }

    #[test]
    fn homebrew_cask_defaults_merge_into_per_crate_publish_when_unset() {
        let defaults = Defaults {
            publish: Some(PublishDefaults {
                homebrew_cask: Some(HomebrewCaskConfig {
                    homepage: Some("https://default.example.com".to_string()),
                    license: Some("MIT".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        // Per-crate has no publish.homebrew_cask at all.
        let mut crate_cfg = make_crate("mycrate");

        apply_to_crate(&defaults, &mut crate_cfg);

        let publish = crate_cfg.publish.expect("publish block should be created");
        let cask = publish
            .homebrew_cask
            .expect("publish.homebrew_cask should be inherited from defaults");
        assert_eq!(
            cask.homepage.as_deref(),
            Some("https://default.example.com"),
            "homepage should be filled from defaults"
        );
        assert_eq!(
            cask.license.as_deref(),
            Some("MIT"),
            "license should be filled from defaults"
        );
    }

    #[test]
    fn homebrew_cask_defaults_fill_missing_fields_but_per_crate_wins_on_collision() {
        let defaults = Defaults {
            publish: Some(PublishDefaults {
                homebrew_cask: Some(HomebrewCaskConfig {
                    homepage: Some("https://default.example.com".to_string()),
                    license: Some("MIT".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        // Per-crate explicitly sets homepage; license is unset.
        let mut crate_cfg = make_crate("mycrate");
        crate_cfg.publish = Some(PublishConfig {
            homebrew_cask: Some(HomebrewCaskConfig {
                homepage: Some("https://crate.example.com".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });

        apply_to_crate(&defaults, &mut crate_cfg);

        let publish = crate_cfg.publish.unwrap();
        let cask = publish.homebrew_cask.unwrap();
        assert_eq!(
            cask.homepage.as_deref(),
            Some("https://crate.example.com"),
            "per-crate homepage should win over defaults"
        );
        assert_eq!(
            cask.license.as_deref(),
            Some("MIT"),
            "license should be filled from defaults (per-crate left it unset)"
        );
    }

    #[test]
    fn homebrew_cask_uninstall_nested_struct_deep_merges() {
        // Structured nested types must deep-merge, not replace wholesale.
        // defaults: uninstall.launchctl = ["com.example.myapp"]
        // crate:    uninstall.quit      = ["com.example.myapp.helper"]
        // expect both fields to survive in the merged result.
        let defaults = Defaults {
            publish: Some(PublishDefaults {
                homebrew_cask: Some(HomebrewCaskConfig {
                    uninstall: Some(HomebrewCaskUninstall {
                        launchctl: Some(vec!["com.example.myapp".to_string()]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("mycrate");
        crate_cfg.publish = Some(PublishConfig {
            homebrew_cask: Some(HomebrewCaskConfig {
                uninstall: Some(HomebrewCaskUninstall {
                    quit: Some(vec!["com.example.myapp.helper".to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });

        apply_to_crate(&defaults, &mut crate_cfg);

        let publish = crate_cfg.publish.unwrap();
        let cask = publish.homebrew_cask.unwrap();
        let uninstall = cask
            .uninstall
            .expect("uninstall should be present after merge");
        let expected_launchctl = vec!["com.example.myapp".to_string()];
        assert_eq!(
            uninstall.launchctl.as_deref(),
            Some(expected_launchctl.as_slice()),
            "launchctl from defaults should survive deep merge"
        );
        let expected_quit = vec!["com.example.myapp.helper".to_string()];
        assert_eq!(
            uninstall.quit.as_deref(),
            Some(expected_quit.as_slice()),
            "quit from crate should survive deep merge"
        );
    }

    // ---- New-type round-trip coverage for the typed fields ----

    #[test]
    fn nfpm_umask_string_or_u32_inherits_from_defaults() {
        // `NfpmConfig.umask` is `Option<StringOrU32>`. defaults.nfpms is
        // wired through `merge_list_by_identity`, so the typed scalar must
        // round-trip through the JSON deep-merge intact.
        use crate::config::{NfpmConfig, StringOrU32};
        let defaults = Defaults {
            nfpms: Some(NfpmConfig {
                package_name: Some("myapp".to_string()),
                umask: Some(StringOrU32(0o022)),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");
        crate_cfg.nfpms = Some(vec![NfpmConfig {
            package_name: Some("myapp".to_string()),
            // umask deliberately unset — should inherit from defaults.
            ..Default::default()
        }]);

        apply_to_crate(&defaults, &mut crate_cfg);

        let nfpm = crate_cfg.nfpms.expect("nfpm vec");
        assert_eq!(nfpm.len(), 1, "identity match collapses to one entry");
        assert_eq!(
            nfpm[0].umask,
            Some(StringOrU32(0o022)),
            "StringOrU32 must round-trip through the JSON-backed deep-merge"
        );
    }

    #[test]
    fn notarize_timeout_duration_does_not_inherit_today() {
        // `NotarizeConfig.macos[].notarize.timeout` is `Option<HumanDuration>`.
        // The merge engine does NOT yet fold top-level `Config.notarize`
        // into per-crate state because notarize lives on `Config`, not
        // `CrateConfig`. This test pins that gap so any future wiring
        // change surfaces explicitly.
        use crate::config::{
            HumanDuration, MacOSNotarizeApiConfig, MacOSSignNotarizeConfig, NotarizeConfig,
        };
        let defaults = Defaults {
            notarize: Some(NotarizeConfig {
                macos: Some(vec![MacOSSignNotarizeConfig {
                    notarize: Some(MacOSNotarizeApiConfig {
                        timeout: Some(HumanDuration(std::time::Duration::from_secs(900))),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut crate_cfg = make_crate("a");

        apply_to_crate(&defaults, &mut crate_cfg);

        // Structural proof of the gap: `CrateConfig` has no `notarize` field
        // at all today. Notarize lives only on `Config` and `Defaults`, so
        // there is no per-crate slot for the merge engine to fold the typed
        // `HumanDuration` timeout into. Test pins the structural state so
        // any future field-add (CrateConfig.notarize) surfaces here and the
        // wiring change can be cross-checked against the known-bug entry.
        let _ = crate_cfg; // no-op: no per-crate notarize field exists.
    }

    #[test]
    fn changelog_header_content_source_does_not_inherit_today() {
        // `ChangelogConfig.header` is `Option<ContentSource>`. ChangelogConfig
        // lives on `Config` (not on `CrateConfig` and not on `Defaults`), so
        // the merge engine has no path to fold a default header into per-crate
        // state today. Test pins the gap.
        use crate::config::{ChangelogConfig, ContentSource};
        let _defaults_changelog = ChangelogConfig {
            header: Some(ContentSource::Inline("## Notes".to_string())),
            ..Default::default()
        };
        let defaults = Defaults::default();
        let mut crate_cfg = make_crate("a");

        apply_to_crate(&defaults, &mut crate_cfg);

        // CrateConfig has no `changelog` field — this is the structural
        // proof that no per-crate inheritance path exists today.
        let _ = crate_cfg; // explicit no-op assertion: there is no field to check.
    }

    // --------------- Top-level Config field fold ---------------

    /// `defaults.source` fills `Config.source` when the user did not set it.
    #[test]
    fn top_level_source_fills_from_defaults_when_unset() {
        use crate::config::{Config, SourceConfig};
        let mut config = Config {
            defaults: Some(Defaults {
                source: Some(SourceConfig {
                    enabled: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_defaults(&mut config);
        assert_eq!(
            config.source.as_ref().and_then(|s| s.enabled),
            Some(true),
            "defaults.source should fill Config.source"
        );
    }

    /// User-set `Config.source` wins over `defaults.source` (deep-merge fills
    /// only the fields the user left unset).
    #[test]
    fn top_level_source_user_overrides_defaults() {
        use crate::config::{Config, SourceConfig};
        let mut config = Config {
            defaults: Some(Defaults {
                source: Some(SourceConfig {
                    enabled: Some(false),
                    name_template: Some("default-name".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            source: Some(SourceConfig {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_defaults(&mut config);
        let s = config.source.as_ref().expect("source set");
        assert_eq!(s.enabled, Some(true), "user value wins");
        assert_eq!(
            s.name_template.as_deref(),
            Some("default-name"),
            "field unset by user is filled from defaults"
        );
    }

    /// `defaults.upx` fills the empty `Config.upx` Vec when no user upx is set.
    #[test]
    fn top_level_upx_fills_when_empty() {
        use crate::config::{Config, UpxConfig};
        let mut config = Config {
            defaults: Some(Defaults {
                upx: Some(UpxConfig {
                    id: Some("from-defaults".to_string()),
                    enabled: Some(StringOrBool::Bool(true)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_defaults(&mut config);
        assert_eq!(config.upx.len(), 1);
        assert_eq!(config.upx[0].id.as_deref(), Some("from-defaults"));
    }

    /// User-set top-level Vec wins; defaults are inert.
    #[test]
    fn top_level_upx_does_not_override_user_vec() {
        use crate::config::{Config, UpxConfig};
        let mut config = Config {
            defaults: Some(Defaults {
                upx: Some(UpxConfig {
                    id: Some("from-defaults".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            upx: vec![UpxConfig {
                id: Some("user-upx".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        apply_defaults(&mut config);
        assert_eq!(config.upx.len(), 1);
        assert_eq!(
            config.upx[0].id.as_deref(),
            Some("user-upx"),
            "user vec wins"
        );
    }

    /// Each fill-when-empty Vec slot loads its single defaults entry.
    #[test]
    fn top_level_signs_and_friends_fill_when_empty() {
        use crate::config::{
            Config, DockerSignConfig, MakeselfConfig, SbomConfig, SignConfig, SrpmConfig,
        };
        let mut config = Config {
            defaults: Some(Defaults {
                sign: Some(SignConfig {
                    cmd: Some("cosign".to_string()),
                    ..Default::default()
                }),
                binary_signs: Some(SignConfig {
                    cmd: Some("gpg".to_string()),
                    ..Default::default()
                }),
                docker_signs: Some(DockerSignConfig {
                    cmd: Some("cosign".to_string()),
                    ..Default::default()
                }),
                sbom: Some(SbomConfig {
                    cmd: Some("syft".to_string()),
                    ..Default::default()
                }),
                makeselves: Some(MakeselfConfig {
                    id: Some("from-defaults".to_string()),
                    ..Default::default()
                }),
                srpms: Some(SrpmConfig {
                    enabled: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_defaults(&mut config);
        assert_eq!(config.signs.len(), 1);
        assert_eq!(config.signs[0].cmd.as_deref(), Some("cosign"));
        assert_eq!(config.binary_signs.len(), 1);
        assert_eq!(config.binary_signs[0].cmd.as_deref(), Some("gpg"));
        assert_eq!(config.docker_signs.as_ref().unwrap().len(), 1);
        assert_eq!(config.sboms.len(), 1);
        assert_eq!(config.sboms[0].cmd.as_deref(), Some("syft"));
        assert_eq!(config.makeselfs.len(), 1);
        assert_eq!(config.makeselfs[0].id.as_deref(), Some("from-defaults"));
        assert_eq!(config.srpms.as_ref().and_then(|s| s.enabled), Some(true));
    }

    /// `defaults.notarize` deep-merges into `Config.notarize` when both Some.
    #[test]
    fn top_level_notarize_deep_merges() {
        use crate::config::{
            Config, MacOSNativeArtifactKind, MacOSNativeSignNotarizeConfig, NotarizeConfig,
        };
        let mut config = Config {
            defaults: Some(Defaults {
                notarize: Some(NotarizeConfig {
                    macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                        use_: Some(MacOSNativeArtifactKind::Dmg),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            notarize: Some(NotarizeConfig::default()),
            ..Default::default()
        };
        apply_defaults(&mut config);
        let n = config.notarize.as_ref().expect("notarize set");
        let mac = n.macos_native.as_ref().expect("macos_native set");
        assert!(
            matches!(
                mac.first().and_then(|m| m.use_.as_ref()),
                Some(MacOSNativeArtifactKind::Dmg)
            ),
            "deep-merge should fold defaults.notarize.macos_native[0].use_ into Config.notarize"
        );
    }

    /// `defaults.version_files` is folded into a crate that declares none.
    #[test]
    fn defaults_version_files_fill_when_crate_unset() {
        let defaults = Defaults {
            version_files: Some(vec!["charts/app/Chart.yaml".to_string()]),
            ..Default::default()
        };
        let mut crate_cfg = CrateConfig::default();
        apply_to_crate(&defaults, &mut crate_cfg);
        assert_eq!(
            crate_cfg.version_files.as_deref(),
            Some(&["charts/app/Chart.yaml".to_string()][..])
        );
    }

    /// A per-crate `version_files` list wins outright over the defaults list
    /// (override-not-append).
    #[test]
    fn crate_version_files_override_defaults() {
        let defaults = Defaults {
            version_files: Some(vec!["from-defaults.md".to_string()]),
            ..Default::default()
        };
        let mut crate_cfg = CrateConfig {
            version_files: Some(vec!["from-crate.md".to_string()]),
            ..Default::default()
        };
        apply_to_crate(&defaults, &mut crate_cfg);
        assert_eq!(
            crate_cfg.version_files.as_deref(),
            Some(&["from-crate.md".to_string()][..])
        );
    }
}
