//! Chocolatey publisher — assemble a `.nuspec` + `chocolateyinstall.ps1`,
//! pack a native nupkg (OPC/ZIP), and push to the configured NuGet V2 feed.

mod install;
mod nuspec;
pub(crate) mod package;
pub(crate) mod publish;
pub mod publisher;

#[cfg(test)]
mod tests;

pub use install::{InstallScriptDual, generate_install_script, generate_install_script_dual};
pub use nuspec::{NuspecParams, generate_nuspec};
pub use publish::publish_to_chocolatey;
pub(crate) use publish::{render_nuspec_for_crate, validate_install_mode_for_crate};
pub use publisher::ChocolateyPublisher;
pub(crate) use publisher::is_chocolatey_per_crate_configured;
