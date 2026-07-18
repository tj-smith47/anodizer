//! Gitea release backend — creates releases, uploads assets via the Gitea API.
//!
//! Gitea's release API is simpler than GitLab's: assets are uploaded directly
//! via multipart POST to the release endpoint (no package registry indirection).
//! Draft support is limited (Gitea has it but the release client treats
//! `PublishRelease` as a no-op), so we follow that same approach.
//!
//! Gitea release backend.
//!
//! ## Note on commit 4a9d25f (default-branch fallback)
//!
//! A `CreateFile` path previously hard-coded
//! `master` when the server-side default-branch lookup failed. Anodizer
//! does not call Gitea's `repos/{owner}/{repo}/contents/{path}` create-file
//! endpoint — every publisher (homebrew, scoop, krew, nix, aur, …) targets
//! Gitea via `git clone` + `git push` over SSH/HTTPS, not via the REST
//! contents API. The `branch`-defaulting bug therefore has no surface in
//! anodizer (n/a-by-construction).

use std::path::Path;

use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{
    RetryLog, RetryPolicy, SuccessClass, retry_http_async, retry_http_async_deadline,
};
use anodizer_core::url::percent_encode_path_segment as encode_segment;
use anyhow::{Context as _, Result, bail};
use reqwest::Client;

use crate::release_body::compose_body_for_mode;

mod assets;
mod backend;
mod client;
mod release;
mod types;
mod url;

pub(crate) use assets::*;
pub(crate) use backend::*;
pub(crate) use client::*;
pub(crate) use release::*;
pub(crate) use types::*;
pub(crate) use url::*;

#[cfg(test)]
mod tests;
