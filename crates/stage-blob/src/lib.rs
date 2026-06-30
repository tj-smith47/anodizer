//! Blob stage — uploads release artifacts to S3 / GCS / Azure Blob.
//!
//! - [`Provider`] — `s3` / `gs` / `azblob` selection.
//! - [`BlobStage`] — the [`anodizer_core::stage::Stage`] driver. Each config is
//!   prepared serially (template render, store build, KMS preflight) before the
//!   parallel upload, so credential/KMS errors surface before any bytes leave.

mod kms;
mod preflight;
mod provider;
pub mod publisher;
mod run;
mod store;
mod upload;

#[cfg(test)]
mod tests;

pub use provider::Provider;
pub use publisher::BlobPublisher;
pub use run::BlobStage;

/// Environment requirements for the blob stage, derived per `blobs:` entry:
///
/// * `s3` with a custom `endpoint` (MinIO-style) — the static keypair
///   (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`), any env vars the
///   endpoint template references, and reachability of the rendered
///   endpoint. Plain AWS (and `gs` / `azblob`) declare no credential
///   requirement: those providers resolve ambient chains (instance
///   metadata, workload identity, profiles) that are not env-observable,
///   and requiring env vars there would false-fail valid setups.
/// * a `kms_key` using a CLI-backed scheme — the matching cloud CLI
///   (`aws` / `gcloud` / `az`).
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    use anodizer_core::env_preflight::template_env_refs;
    let mut out = Vec::new();
    for c in anodizer_core::env_preflight::crate_universe(&ctx.config) {
        for b in c.blobs.iter().flatten() {
            // Unknown providers are config-validation territory, not preflight's.
            let Ok(provider) = Provider::parse(&b.provider) else {
                continue;
            };
            if provider == Provider::S3
                && let Some(endpoint) = b.endpoint.as_deref()
            {
                out.push(anodizer_core::EnvRequirement::EnvAllOf {
                    vars: vec![
                        "AWS_ACCESS_KEY_ID".to_string(),
                        "AWS_SECRET_ACCESS_KEY".to_string(),
                    ],
                });
                let refs = template_env_refs(endpoint);
                if !refs.is_empty() {
                    out.push(anodizer_core::EnvRequirement::EnvAllOf { vars: refs });
                }
                if let Ok(rendered) = anodizer_core::template::render(endpoint, ctx.template_vars())
                    && !rendered.trim().is_empty()
                {
                    out.push(anodizer_core::EnvRequirement::Endpoint { url: rendered });
                }
            }
            if let Some(kms_key) = b.kms_key.as_deref() {
                let tool = match crate::kms::parse_kms_provider(kms_key) {
                    crate::kms::KmsProvider::Aws => Some("aws"),
                    crate::kms::KmsProvider::Gcp => Some("gcloud"),
                    crate::kms::KmsProvider::Azure => Some("az"),
                    _ => None,
                };
                if let Some(tool) = tool {
                    out.push(anodizer_core::EnvRequirement::Tool {
                        name: tool.to_string(),
                    });
                }
            }
        }
    }
    out
}
