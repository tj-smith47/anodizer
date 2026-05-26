//! GemFury (fury.io) publisher — pushes deb/rpm/apk artifacts to
//! `https://push.fury.io/<account>` via HTTP Basic auth (push token as
//! username, empty password) and supports per-version rollback via
//! `DELETE https://api.fury.io/<account>/packages/<name>/versions/<version>`.
//!
//! Mirrors GoReleaser Pro v2.14's `gemfury:` block (closed source — surface
//! inferred from `https://goreleaser.com/customization/publish/gemfury/`
//! and Fury's public REST API).

pub mod publish;
pub mod publisher;

#[cfg(test)]
mod tests;

pub use publish::{GemFuryTarget, publish_to_gemfury};
pub use publisher::GemFuryPublisher;
