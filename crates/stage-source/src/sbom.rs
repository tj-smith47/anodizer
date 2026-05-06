//! SBOM generation (CycloneDX 1.5 + SPDX 2.3) plus Cargo.lock parsing.

use anyhow::{Context as _, Result};

// ---------------------------------------------------------------------------
// SBOM generation
// ---------------------------------------------------------------------------

/// A parsed Cargo.lock package entry.
#[derive(Debug, Clone)]
pub struct CargoPackage {
    pub name: String,
    pub version: String,
    pub source: Option<String>,
}

/// Parse `Cargo.lock` to extract package entries.
pub fn parse_cargo_lock(content: &str) -> Result<Vec<CargoPackage>> {
    let parsed: toml::Value =
        toml::from_str(content).context("sbom: failed to parse Cargo.lock as TOML")?;

    let packages = parsed
        .get("package")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let name = entry.get("name")?.as_str()?.to_string();
                    let version = entry.get("version")?.as_str()?.to_string();
                    let source = entry
                        .get("source")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string());
                    Some(CargoPackage {
                        name,
                        version,
                        source,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(packages)
}

/// Generate a CycloneDX 1.5 SBOM in JSON format.
///
/// `timestamp` is embedded in `metadata.timestamp` and must be supplied by the
/// caller so that repeated pipeline runs (e.g. anodizer-action retries) emit
/// byte-identical output. Callers should derive it from `ctx.template_vars()`
/// (`CommitDate`) so the value is tied to the release tag, not wall-clock.
pub fn generate_cyclonedx(
    project_name: &str,
    version: &str,
    timestamp: &str,
    packages: &[CargoPackage],
) -> Result<serde_json::Value> {
    let components: Vec<serde_json::Value> = packages
        .iter()
        .map(|pkg| {
            let mut component = serde_json::json!({
                "type": "library",
                "name": pkg.name,
                "version": pkg.version,
                "purl": format!("pkg:cargo/{}@{}", pkg.name, pkg.version),
            });

            if let Some(ref source) = pkg.source
                && source.starts_with("registry+")
            {
                component["externalReferences"] = serde_json::json!([
                    {
                        "type": "distribution",
                        "url": format!("https://crates.io/crates/{}/{}", pkg.name, pkg.version)
                    }
                ]);
            }

            component
        })
        .collect();

    let sbom = serde_json::json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "timestamp": timestamp,
            "component": {
                "type": "application",
                "name": project_name,
                "version": version,
            },
            "tools": {
                "components": [
                    {
                        "type": "application",
                        "name": "anodizer",
                        "publisher": "anodizer",
                    }
                ]
            }
        },
        "components": components,
    });

    Ok(sbom)
}

/// Generate an SPDX 2.3 SBOM in JSON format.
///
/// `timestamp` populates `creationInfo.created`; `namespace_uuid` populates the
/// trailing segment of `documentNamespace`. Both are caller-supplied so the
/// output is byte-identical across repeated pipeline runs (release asset
/// uploads are non-idempotent when the file bytes differ from a prior
/// upload — GitHub's ReleaseAsset API rejects re-uploads with `already_exists`
/// when sizes diverge).
pub fn generate_spdx(
    project_name: &str,
    version: &str,
    timestamp: &str,
    namespace_uuid: &str,
    packages: &[CargoPackage],
) -> Result<serde_json::Value> {
    // The root package
    let root_package = serde_json::json!({
        "SPDXID": "SPDXRef-Package",
        "name": project_name,
        "versionInfo": version,
        "downloadLocation": "NOASSERTION",
        "filesAnalyzed": false,
    });

    let mut spdx_packages = vec![root_package];
    let mut relationships = vec![serde_json::json!({
        "spdxElementId": "SPDXRef-DOCUMENT",
        "relatedSpdxElement": "SPDXRef-Package",
        "relationshipType": "DESCRIBES",
    })];

    for (i, pkg) in packages.iter().enumerate() {
        let spdx_id = format!("SPDXRef-Package-{}", i);

        let download_location = if let Some(ref source) = pkg.source {
            if source.starts_with("registry+") {
                format!("https://crates.io/crates/{}/{}", pkg.name, pkg.version)
            } else {
                source.clone()
            }
        } else {
            "NOASSERTION".to_string()
        };

        let pkg_entry = serde_json::json!({
            "SPDXID": spdx_id,
            "name": pkg.name,
            "versionInfo": pkg.version,
            "downloadLocation": download_location,
            "filesAnalyzed": false,
            "externalRefs": [
                {
                    "referenceCategory": "PACKAGE-MANAGER",
                    "referenceType": "purl",
                    "referenceLocator": format!("pkg:cargo/{}@{}", pkg.name, pkg.version),
                }
            ],
        });

        spdx_packages.push(pkg_entry);

        relationships.push(serde_json::json!({
            "spdxElementId": "SPDXRef-Package",
            "relatedSpdxElement": spdx_id,
            "relationshipType": "DEPENDS_ON",
        }));
    }

    let sbom = serde_json::json!({
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": format!("{}-{}", project_name, version),
        "documentNamespace": format!(
            "https://spdx.org/spdxdocs/{}-{}-{}",
            project_name, version, namespace_uuid,
        ),
        "creationInfo": {
            "created": timestamp,
            "creators": ["Tool: anodizer"],
        },
        "packages": spdx_packages,
        "relationships": relationships,
    });

    Ok(sbom)
}

/// Deterministic UUID v4-shaped identifier derived from `seed`.
///
/// Same seed always produces the same UUID. Not cryptographic — the value is
/// only used as the trailing component of an SPDX `documentNamespace`, where
/// the purpose is per-document uniqueness within a project, not secrecy.
///
/// Note: `DefaultHasher` output is not stable across Rust versions, so the
/// same `seed` may produce different UUIDs when compiled with different Rust
/// toolchains. Determinism is only guaranteed within a single toolchain, which
/// is all the release-pipeline idempotency path needs.
pub fn deterministic_uuid_from(seed: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h1 = DefaultHasher::new();
    seed.hash(&mut h1);
    "anodizer-sbom-ns-v1".hash(&mut h1);
    let h1 = h1.finish();

    let mut h2 = DefaultHasher::new();
    seed.hash(&mut h2);
    "anodizer-sbom-ns-v2".hash(&mut h2);
    let h2 = h2.finish();

    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (h1 >> 32) as u32,
        (h1 >> 16) as u16,
        h1 as u16 & 0x0FFF,
        (h2 >> 48) as u16 & 0x3FFF | 0x8000,
        h2 & 0xFFFF_FFFF_FFFF,
    )
}
