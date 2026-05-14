//! Snapcraft build + publish stages.
//!
//! - [`SnapcraftStage`]: pre-stages binaries into a prime directory, writes
//!   `snap.yaml`, then runs `snapcraft pack` per platform group.
//! - [`SnapcraftPublishStage`]: uploads built `.snap` artifacts via
//!   `snapcraft upload --release=...` and records its own
//!   `PublisherResult` directly into `ctx.publish_report` (mirrors the
//!   BlobStage pattern; see
//!   `.claude/audits/2026-05-15-release-resilience-review.md` finding
//!   C3 for why a trait-based `SnapcraftPublisher` would double-publish).

mod arch;
mod build_stage;
mod command;
mod generate;
mod publish_stage;
mod targets;
mod yaml;

#[cfg(test)]
mod tests;

pub use build_stage::SnapcraftStage;
pub use command::{
    is_retriable_snap_push, resolve_effective_channels, snapcraft_command, snapcraft_upload_command,
};
pub use generate::generate_snap_yaml;
pub use publish_stage::SnapcraftPublishStage;
