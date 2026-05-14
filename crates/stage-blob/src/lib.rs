//! Blob stage — uploads release artifacts to S3 / GCS / Azure Blob.
//!
//! - [`Provider`] — `s3` / `gs` / `azblob` selection.
//! - [`BlobStage`] — the [`Stage`] driver: per-config Phase 1 (template
//!   render, store build, KMS preflight) → Phase 2 parallel upload.

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
