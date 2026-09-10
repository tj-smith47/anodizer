use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util;

mod manifest;
mod publish;
mod publisher;
mod render;

pub(crate) use manifest::*;
pub use publish::*;
pub(crate) use publisher::*;
pub(crate) use render::*;

/// Why a manifest entry is disqualified when `repository:` names no
/// `owner`/`name` pair: the bucket has nowhere to land, but every sibling
/// crate can still publish, so the entry is skipped rather than failing the
/// publisher.
pub(crate) const MISSING_REPOSITORY_REASON: &str = "repository.name is not set";

#[cfg(test)]
mod publish_flow_tests;
#[cfg(test)]
mod publisher_tests;
#[cfg(test)]
mod tests;
