//! GemFury (fury.io) publisher — pushes deb/rpm/apk artifacts to
//! `https://push.fury.io/<account>` via HTTP Basic auth (push token as
//! username, empty password) and supports per-version rollback via
//! `DELETE https://api.fury.io/<account>/packages/<name>/versions/<version>`.
//!
//! Implements the `gemfury:` publisher block: push deb/rpm/apk packages
//! to a Gemfury account.

pub mod publish;
pub mod publisher;

#[cfg(test)]
mod tests;

pub use publish::{GemFuryTarget, publish_to_gemfury};
pub use publisher::GemFuryPublisher;
