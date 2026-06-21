//! Deterministic MSI ProductCode derivation.
//!
//! Windows treats each *version* of a product as a distinct entity keyed by its
//! ProductCode GUID, while the stable UpgradeCode anchors the upgrade lineage
//! (see `MajorUpgrade` in `packaging/anodizer.wxs`). anodizer therefore derives
//! a ProductCode that is **stable for a given `(project, version, msi_arch)`**
//! but **rotates per version** — exactly the contract `Product Id="*"` (random
//! per build) cannot satisfy, and which winget needs for AppsAndFeaturesEntries
//! upgrade detection.
//!
//! The value is a UUIDv5 (RFC 4122 §4.3, SHA-1 namespaced) over the key
//! `{ProjectName}\0{Version}\0{MsiArch}`. The NUL separators make the field
//! boundaries unambiguous so two distinct triples can never collide by
//! concatenation (e.g. `("ab", "c")` vs `("a", "bc")`).

use uuid::Uuid;

/// Anodizer's fixed UUIDv5 namespace for MSI ProductCode derivation.
///
/// Reuses the `.wxs` UpgradeCode GUID verbatim as the namespace so the two
/// stable GUIDs share a single documented constant: the UpgradeCode anchors the
/// product lineage and is also the namespace under which every per-version
/// ProductCode in that lineage is derived.
const PRODUCT_CODE_NAMESPACE: Uuid = Uuid::from_u128(0x6f3a2b1c_0d4e_5a6b_7c8d_9e0f1a2b3c4d);

/// Derive the deterministic MSI ProductCode for a `(project, version, arch)`
/// triple, formatted as the upper-case, brace-wrapped GUID that Windows
/// Installer and winget manifests expect (e.g. `{A1B2C3D4-...}`).
///
/// Stable for identical inputs and distinct across any differing field; the
/// version component guarantees a fresh ProductCode per release.
pub fn derive_product_code(project_name: &str, version: &str, msi_arch: &str) -> String {
    let key = format!("{project_name}\0{version}\0{msi_arch}");
    let uuid = Uuid::new_v5(&PRODUCT_CODE_NAMESPACE, key.as_bytes());
    format!("{{{}}}", uuid.hyphenated().to_string().to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unwrap_guid(s: &str) -> &str {
        s.strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .expect("product code is brace-wrapped")
    }

    #[test]
    fn product_code_is_braced_upper_guid() {
        let pc = derive_product_code("anodizer", "1.0.0", "x64");
        assert!(pc.starts_with('{') && pc.ends_with('}'), "got {pc}");
        let inner = unwrap_guid(&pc);
        // Canonical 8-4-4-4-12 hyphenated GUID, upper-cased.
        assert_eq!(inner.len(), 36, "got {inner}");
        assert_eq!(inner, inner.to_uppercase(), "must be upper-case");
        Uuid::parse_str(inner).expect("inner is a valid UUID");
    }

    #[test]
    fn product_code_is_stable_for_same_inputs() {
        assert_eq!(
            derive_product_code("anodizer", "1.0.0", "x64"),
            derive_product_code("anodizer", "1.0.0", "x64"),
        );
    }

    #[test]
    fn product_code_differs_per_version() {
        assert_ne!(
            derive_product_code("anodizer", "1.0.0", "x64"),
            derive_product_code("anodizer", "1.0.1", "x64"),
        );
    }

    #[test]
    fn product_code_differs_per_arch() {
        assert_ne!(
            derive_product_code("anodizer", "1.0.0", "x64"),
            derive_product_code("anodizer", "1.0.0", "arm64"),
        );
    }

    #[test]
    fn product_code_differs_per_project() {
        assert_ne!(
            derive_product_code("anodizer", "1.0.0", "x64"),
            derive_product_code("cfgd", "1.0.0", "x64"),
        );
    }

    #[test]
    fn product_code_field_separator_prevents_concatenation_collision() {
        // Without the NUL separator ("ab"+"c"+arch) and ("a"+"bc"+arch) would
        // hash the same byte stream. The separator must keep them distinct.
        assert_ne!(
            derive_product_code("ab", "c", "x64"),
            derive_product_code("a", "bc", "x64"),
        );
    }

    #[test]
    fn product_code_uses_upgradecode_namespace() {
        // The derivation namespace IS the .wxs UpgradeCode GUID. Recomputing the
        // UUIDv5 against a literal-parsed copy of that GUID must match, pinning
        // the documented relationship between the two constants.
        let ns = Uuid::parse_str("6f3a2b1c-0d4e-5a6b-7c8d-9e0f1a2b3c4d").unwrap();
        let key = format!("anodizer\0{}\0x64", "1.0.0");
        let expected = format!(
            "{{{}}}",
            Uuid::new_v5(&ns, key.as_bytes())
                .hyphenated()
                .to_string()
                .to_uppercase()
        );
        assert_eq!(derive_product_code("anodizer", "1.0.0", "x64"), expected);
    }
}
