//! Blob stage — uploads release artifacts to S3 / GCS / Azure Blob.
//!
//! - [`Provider`] — `s3` / `gs` / `azblob` selection.
//! - [`BlobStage`] — the [`anodizer_core::stage::Stage`] driver. Each config is
//!   prepared serially (template render, store build, KMS preflight) before the
//!   parallel upload, so credential/KMS errors surface before any bytes leave.

mod kms;
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
