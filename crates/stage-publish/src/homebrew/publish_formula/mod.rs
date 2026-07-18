//! `publish_to_homebrew` — per-crate formula (and optional same-tap cask)
//! publisher.

mod publish;
mod render;

pub use publish::*;
pub(crate) use render::*;

#[cfg(test)]
mod tests;
