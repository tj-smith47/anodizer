use std::sync::LazyLock;

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::template::{self, TemplateVars};
use anodizer_core::util::static_regex;
use anyhow::{Context as _, Result};
use regex::Regex;
use serde::Serialize;

use crate::util;

mod fields;
mod identifier;
mod installers;
mod manifest;
mod publish;
mod render;

pub(crate) use fields::*;
pub use identifier::*;
pub(crate) use installers::*;
pub(crate) use manifest::*;
pub use publish::*;
pub(crate) use render::*;

#[cfg(test)]
mod publisher_tests;
#[cfg(test)]
mod tests;
