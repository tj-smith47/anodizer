use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;
use anodizer_core::hashing::sha256_file;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryLog, RetryPolicy, SuccessClass, retry_http_blocking_deadline};
use anyhow::{Context as _, Result, bail};
use std::collections::HashMap;
use std::fs;

mod client;
mod publisher;
mod rollback;
mod upload;

pub use client::*;
pub(crate) use publisher::*;
pub(crate) use rollback::*;
pub use upload::*;

#[cfg(test)]
mod preflight_live_tests;
#[cfg(test)]
mod publisher_tests;
#[cfg(test)]
mod tests;
