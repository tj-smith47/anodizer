use anodizer_core::config::{CargoPublishConfig, CrateConfig, WaitForWorkspaceDepsConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::util::topological_sort;
use anyhow::{Context as _, Result};
use std::collections::{HashMap, HashSet};
use std::process::Command;

/// Default seconds to wait for a freshly-published crate to appear in the
/// crates.io sparse index. Mirrors the historical anodizer default; only
/// matters when the crate has dependents that need it published first.
const DEFAULT_INDEX_TIMEOUT_SECS: u64 = 300;

/// How many times to retry `cargo publish` when it fails with a signature
/// that smells like sparse-index propagation lag (see
/// [`is_index_propagation_failure`]). Three total attempts (the initial
/// publish plus two retries) covers the common case where the dependent's
/// `cargo publish` lands on a stale CDN edge a beat after [`poll_crates_io_index`]
/// already saw the previous crate confirmed on a different edge. Higher
/// attempt counts buy nothing: by then either Fastly has fanned out or the
/// failure isn't propagation-related.
const PUBLISH_PROPAGATION_RETRIES: u32 = 3;

/// Backoff between propagation-retry attempts. Short by design — the outer
/// [`poll_crates_io_index`] already burned the propagation budget waiting
/// for OUR edge to confirm; this is just for inter-edge skew where cargo's
/// invocation races against Fastly's broadcast.
const PUBLISH_PROPAGATION_BACKOFF: std::time::Duration = std::time::Duration::from_secs(15);

mod already_published;
mod command;
mod index;
mod manifest;
mod oidc;
mod plan;
mod publish;
mod publisher;
mod retry;
mod workspace_deps;

pub(crate) use already_published::*;
pub(crate) use command::*;
pub(crate) use index::*;
pub(crate) use manifest::*;
pub(crate) use plan::*;
pub(crate) use publish::*;
pub(crate) use publisher::*;
pub(crate) use retry::*;
pub(crate) use workspace_deps::*;

pub use index::{published_on_crates_io, targets_crates_io};
pub use publish::publish_to_cargo;
pub use publisher::CargoPublisher;

#[cfg(test)]
mod binstall_on_publish_tests;
#[cfg(test)]
mod dep_guard_tests;
#[cfg(all(test, unix))]
mod partial_rollback_tests;
#[cfg(test)]
mod publisher_tests;
#[cfg(test)]
mod tests;
