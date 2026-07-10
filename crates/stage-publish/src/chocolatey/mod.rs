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

/// Resolve the community-gallery package id for a crate's `publish.chocolatey`
/// block without a template context: the configured `name` override, falling
/// back to the crate name — the same judgment the publisher's target
/// collection applies. Returns `None` when the override is a template
/// expression, which cannot be resolved outside a release run (failure
/// recovery tooling probing the gallery must then skip rather than guess).
///
/// Public for the same reason as
/// [`crate::cargo::targets_crates_io`]: `tag rollback`'s published-state
/// guard must name the same package the publisher would push.
pub fn static_package_id(
    crate_name: &str,
    cfg: &anodizer_core::config::ChocolateyConfig,
) -> Option<String> {
    let id = cfg.name.as_deref().unwrap_or(crate_name);
    (!id.contains("{{")).then(|| id.to_string())
}

/// Default push endpoint for the Chocolatey community repository.
pub const COMMUNITY_PUSH_SOURCE: &str = "https://push.chocolatey.org/";

/// Resolve the feed a crate's `publish.chocolatey` block pushes to: the
/// configured `source_repo`, defaulting to the community repository.
pub fn push_source(cfg: &anodizer_core::config::ChocolateyConfig) -> &str {
    cfg.source_repo.as_deref().unwrap_or(COMMUNITY_PUSH_SOURCE)
}

/// Whether the block's push target is the Chocolatey community repository —
/// the only feed with a human-moderation queue whose pending submissions
/// consume a version. Private/self-hosted feeds have no such queue, so
/// community-gallery probes carry no signal for them.
///
/// Public for the same reason as [`static_package_id`]: `tag rollback`'s
/// published-state guard must agree with the publisher about where a
/// package would be pushed.
pub fn targets_community_gallery(cfg: &anodizer_core::config::ChocolateyConfig) -> bool {
    push_source(cfg)
        .trim_end_matches('/')
        .eq_ignore_ascii_case(COMMUNITY_PUSH_SOURCE.trim_end_matches('/'))
}
pub use nuspec::{NuspecParams, generate_nuspec};
pub use publish::publish_to_chocolatey;
pub(crate) use publish::{render_nuspec_for_crate, validate_install_mode_for_crate};
pub use publisher::ChocolateyPublisher;
pub(crate) use publisher::is_chocolatey_per_crate_configured;

#[cfg(test)]
mod gallery_target_tests {
    use anodizer_core::config::ChocolateyConfig;

    #[test]
    fn default_source_repo_targets_community_gallery() {
        let cfg = ChocolateyConfig::default();
        assert_eq!(super::push_source(&cfg), super::COMMUNITY_PUSH_SOURCE);
        assert!(super::targets_community_gallery(&cfg));
    }

    #[test]
    fn explicit_community_source_matches_despite_case_and_slash_variance() {
        for spelled in [
            "https://push.chocolatey.org/",
            "https://push.chocolatey.org",
            "HTTPS://PUSH.CHOCOLATEY.ORG/",
        ] {
            let cfg = ChocolateyConfig {
                source_repo: Some(spelled.to_string()),
                ..Default::default()
            };
            assert!(
                super::targets_community_gallery(&cfg),
                "{spelled} is the community gallery"
            );
        }
    }

    #[test]
    fn private_feed_is_not_community_gallery() {
        let cfg = ChocolateyConfig {
            source_repo: Some("https://nuget.internal.example/v2/".to_string()),
            ..Default::default()
        };
        assert_eq!(
            super::push_source(&cfg),
            "https://nuget.internal.example/v2/"
        );
        assert!(!super::targets_community_gallery(&cfg));
    }
}
