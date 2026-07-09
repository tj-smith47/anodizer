//! GitHub release backend.
//!
//! [`run_github_backend`] is the body of the `ScmTokenType::GitHub` match arm
//! in the dispatcher loop, lifted out of `run.rs` for readability. The
//! module is split into focused submodules:
//!
//! - [`backend`] — the `run_github_backend` orchestrator.
//! - [`lookup`] — Releases-API read paths (draft search, tag lookup,
//!   published-asset enumeration, readiness probing, retention listing).
//! - [`spec`] — the backend's argument-cluster structs and the I/O-free
//!   decision helpers.
//! - [`upload`] — the per-asset upload retry/recovery loop.
//!
//! The per-tool helper modules (`assets`, `client`, `rate_limit`,
//! `retry_call`, `secondary_rate_limit`, `upload_outcome`) host the
//! GitHub-specific helper functions composed by the above.

mod assets;
mod backend;
mod client;
mod lookup;
mod rate_limit;
mod retry_call;
mod retry_classify;
mod secondary_rate_limit;
mod spec;
mod upload;
mod upload_outcome;

pub(crate) use assets::{delete_release_asset_by_name, find_release_asset_probe};
pub(crate) use client::build_octocrab_client;
pub(crate) use rate_limit::check_github_rate_limit_with_env;
pub(crate) use retry_call::{
    is_octocrab_404, is_octocrab_transport_error, octocrab_retry_cause, retry_octocrab_call,
};
use secondary_rate_limit::RetryAfterCapture;

pub(crate) use backend::run_github_backend;
pub use lookup::{PublishedAsset, fetch_published_assets};
pub(crate) use spec::{BackendEnv, GithubReleaseSpec, UploadOpts};

// `upload_retry_locals` is exercised only by `crate::github::upload_retry_locals`
// in the crate's `#[cfg(test)] tests` module; re-export it under the same gate
// so a non-test build doesn't flag the path as unused.
#[cfg(test)]
pub(crate) use spec::upload_retry_locals;
