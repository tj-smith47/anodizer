//! NPM registry publisher.
//!
//! Two distribution modes (see [`anodizer_core::config::NpmMode`]):
//! * `optional-deps` (default for a Rust release): emits npm's native
//!   per-platform packages + a metapackage whose `optionalDependencies` list
//!   them; npm's `os`/`cpu`/`libc` resolution installs only the matching
//!   prebuilt — no download, no postinstall. The pattern leading Rust CLIs
//!   ship binaries through npm with (biome, git-cliff).
//! * `postinstall`: emits a single `package.json` + `postinstall.js` shim that
//!   downloads + sha256-verifies the OS/arch-matching archive at install time
//!   (GoReleaser Pro `npms:` parity).
//!
//! The artifacts already exist (release archives + per-target binaries from
//! the build/archive stages); this publisher wraps them in `npm i`-installable
//! packages and pushes to the configured registry (default
//! `https://registry.npmjs.org`).

mod manifest;
mod optional_deps;
pub mod publish;
pub mod publisher;

#[cfg(test)]
mod tests;

pub use publish::{NpmTarget, publish_to_npm};
pub use publisher::NpmPublisher;
