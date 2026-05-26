//! NPM registry publisher — generates a `package.json` + postinstall
//! shim, packs an npm-shaped `.tgz`, and pushes to the configured
//! registry (default `https://registry.npmjs.org`).
//!
//! Anodizer treats npm as a *publisher-only* distribution channel
//! (precedent: biome, swc, rolldown): the artifacts already exist
//! (release archives produced by `stage-archive`); this publisher
//! wraps them in an `npm i`-installable package whose postinstall
//! script downloads the OS/arch-matching archive at install time.

mod manifest;
pub mod publish;
pub mod publisher;

#[cfg(test)]
mod tests;

pub use publish::{NpmTarget, publish_to_npm};
pub use publisher::NpmPublisher;
