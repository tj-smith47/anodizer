//! GitLab release backend — creates releases, uploads assets, and publishes
//! releases via the GitLab REST API.
//!
//! GitLab does not support draft releases (unlike GitHub), so `PublishRelease`
//! is a no-op.  Asset uploads use either the Generic Package Registry (PUT) or
//! Project Markdown Uploads (POST multipart), then create a release link to
//! the uploaded file.
//!
//! GitLab release backend.

use std::path::Path;

use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{
    RetryLog, RetryPolicy, SuccessClass, retry_http_async, retry_http_async_deadline,
};
use anodizer_core::url::percent_encode_path_segment;
use anodizer_core::{EnvSource, ProcessEnvSource};
use anyhow::{Context as _, Result, bail};
use reqwest::Client;

use crate::release_body::compose_body_for_mode;

mod assets;
mod auth;
mod backend;
mod client;
mod release;
mod types;
mod url;
mod version;

pub(crate) use assets::*;
pub(crate) use auth::*;
pub(crate) use backend::*;
pub(crate) use client::*;
pub(crate) use release::*;
pub(crate) use types::*;
pub(crate) use url::*;
pub(crate) use version::*;

#[cfg(test)]
mod tests;
