mod artifacts;
mod flow;
mod manifest;
mod publish;
mod publisher;

pub use artifacts::*;
use flow::*;
pub(crate) use manifest::*;
pub use publish::*;
pub use publisher::*;

/// Why a plugin entry is disqualified when `repository:` names no
/// `owner`/`name` pair: the plugin index has nowhere to land, but every
/// sibling crate can still publish, so the entry is skipped rather than
/// failing the publisher.
pub(crate) const MISSING_REPOSITORY_REASON: &str = "repository.name is not set";

#[cfg(test)]
mod publisher_tests;
#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests;
