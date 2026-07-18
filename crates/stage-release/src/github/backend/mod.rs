//! The GitHub release orchestrator.
//!
//! [`run_github_backend`] is the body of the `ScmTokenType::GitHub` match arm
//! in the dispatcher loop: it resolves the repo + tag, creates / updates /
//! replaces the release, drives the parallel asset-upload loop with bounded
//! transient retry, publishes the release, and only then runs the
//! nightly-retention sweep (so the new release is live before any prior
//! release is pruned). The lookup, classifier, and client helpers it composes
//! live in the sibling [`super::lookup`], [`super::spec`], and the per-tool
//! helper submodules.

use std::sync::Arc;

use anodizer_core::config::{CrateConfig, ReleaseConfig};
use anyhow::{Context as _, Result};

use super::lookup::{find_draft_by_name, find_release_by_tag, list_releases_by_name};
use super::spec::{
    BackendEnv, GithubReleaseSpec, UploadOpts, check_existing_assets_block_upload,
    nightly_releases_to_prune,
};
use super::{
    build_octocrab_client, check_github_rate_limit_with_env, is_octocrab_404, retry_octocrab_call,
};
use crate::release_body::{
    GITHUB_RELEASE_BODY_MAX_CHARS, build_publish_patch_body, build_release_json,
    compose_body_for_mode,
};
use crate::resolve_release_repo;

mod run;

pub(crate) use run::*;

#[cfg(test)]
mod orchestrator_tests;
