//! Publisher registry — single source of truth for which publishers run.
//!
//! [`configured_publishers`] walks the active [`Context`] and instantiates
//! a `Box<dyn Publisher>` for each configured publisher. The returned slice
//! is what [`crate::dispatch::dispatch`] iterates over.
//!
//! The registry is populated incrementally by the per-publisher migration
//! tasks. The existing macro-driven `PublishStage::run` body continues to
//! dispatch publishers until those migrations are complete; this module +
//! [`crate::dispatch`] live alongside it and are exercised only by tests
//! until the swap lands.

use anodizer_core::context::Context;
use anodizer_core::{Publisher, PublisherGroup};

/// Returns the publishers configured for this release run.
///
/// Walks `ctx.config.crates[*].publish` and the top-level publisher blocks
/// (`dockerhub`, `artifactories`, `cloudsmiths`, `crates[*].blobs`) and
/// instantiates a `Box<dyn Publisher>` for each configured publisher. The
/// returned slice is the single source of truth that
/// [`crate::dispatch::dispatch`] iterates.
///
/// `BlobPublisher` is sourced from the `stage-blob` crate (added as a
/// direct dep — `stage-blob` does not depend on `stage-publish`, so no
/// circular dep is introduced).
pub fn configured_publishers(ctx: &Context) -> Vec<Box<dyn Publisher>> {
    let mut v: Vec<Box<dyn Publisher>> = Vec::new();
    if is_cargo_configured(ctx) {
        v.push(Box::new(crate::cargo::CargoPublisher::new()));
    }
    // Bundle A (Assets group): dockerhub, artifactory, cloudsmith, blob.
    if is_dockerhub_configured(ctx) {
        v.push(Box::new(crate::dockerhub::DockerhubPublisher::new()));
    }
    if is_artifactory_configured(ctx) {
        v.push(Box::new(crate::artifactory::ArtifactoryPublisher::new()));
    }
    if is_cloudsmith_configured(ctx) {
        v.push(Box::new(crate::cloudsmith::CloudsmithPublisher::new()));
    }
    if anodizer_stage_blob::publisher::is_configured(ctx) {
        v.push(Box::new(anodizer_stage_blob::BlobPublisher::new()));
    }
    v
}

/// True when at least one crate in the active config has a
/// `publish.cargo` block. Presence of the block is the opt-in; the
/// per-crate `skip:` template is evaluated later in
/// [`crate::cargo::publish_to_cargo`].
fn is_cargo_configured(ctx: &Context) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().and_then(|p| p.cargo.as_ref()).is_some())
}

/// True when the top-level `dockerhub:` block has at least one entry.
/// `publish_to_dockerhub` short-circuits on an empty vec, so an empty-list
/// keep also returns false here.
fn is_dockerhub_configured(ctx: &Context) -> bool {
    ctx.config.dockerhub.as_ref().is_some_and(|v| !v.is_empty())
}

/// True when the top-level `artifactories:` block has at least one entry.
fn is_artifactory_configured(ctx: &Context) -> bool {
    ctx.config
        .artifactories
        .as_ref()
        .is_some_and(|v| !v.is_empty())
}

/// True when the top-level `cloudsmiths:` block has at least one entry.
fn is_cloudsmith_configured(ctx: &Context) -> bool {
    ctx.config
        .cloudsmiths
        .as_ref()
        .is_some_and(|v| !v.is_empty())
}

/// Group dispatch order: Assets first (uploadable bytes, server-side
/// deletable), then Manager (package-manager state, also reversible), then
/// Submitter (irreversible / moderation-locked: chocolatey, winget, krew).
///
/// The Submitter group runs last so its irreversible publishes can be
/// gated on the success of every reversible publisher that came before
/// it. See [`crate::dispatch::dispatch`] for the gate mechanics.
pub const fn group_dispatch_order() -> [PublisherGroup; 3] {
    [
        PublisherGroup::Assets,
        PublisherGroup::Manager,
        PublisherGroup::Submitter,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    #[test]
    fn configured_publishers_empty_without_publish_blocks() {
        let ctx = Context::test_fixture();
        let publishers = configured_publishers(&ctx);
        assert!(
            publishers.is_empty(),
            "registry should stay empty when no crate opts into a publisher"
        );
    }

    #[test]
    fn cargo_publisher_registered_when_configured() {
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        let publishers = configured_publishers(&ctx);
        assert_eq!(publishers.len(), 1, "exactly one publisher expected");
        assert_eq!(publishers[0].name(), "cargo");
        assert_eq!(publishers[0].group(), PublisherGroup::Submitter);
        assert!(publishers[0].required());
    }

    #[test]
    fn group_dispatch_order_is_assets_manager_submitter() {
        assert_eq!(
            group_dispatch_order(),
            [
                PublisherGroup::Assets,
                PublisherGroup::Manager,
                PublisherGroup::Submitter,
            ]
        );
    }

    #[test]
    fn bundle_a_publishers_registered_when_configured() {
        use anodizer_core::config::{
            ArtifactoryConfig, BlobConfig, CloudSmithConfig, DockerHubConfig,
        };
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            blobs: Some(vec![BlobConfig {
                provider: "s3".to_string(),
                bucket: "my-bucket".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        // Top-level publisher blocks live on Config directly.
        ctx.config.dockerhub = Some(vec![DockerHubConfig {
            username: Some("u".to_string()),
            images: Some(vec!["acme/widget".to_string()]),
            ..Default::default()
        }]);
        ctx.config.artifactories = Some(vec![ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some("https://art.example.com/repo/".to_string()),
            ..Default::default()
        }]);
        ctx.config.cloudsmiths = Some(vec![CloudSmithConfig {
            organization: Some("acme".to_string()),
            repository: Some("widget".to_string()),
            ..Default::default()
        }]);

        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        // Every Bundle A publisher must appear; order is whatever
        // configured_publishers emits (Assets-group registration order).
        for expected in ["dockerhub", "artifactory", "cloudsmith", "blob"] {
            assert!(
                names.contains(&expected),
                "{} missing from registered publishers (got {:?})",
                expected,
                names
            );
            let p = publishers
                .iter()
                .find(|p| p.name() == expected)
                .expect("publisher present");
            assert_eq!(p.group(), PublisherGroup::Assets, "{}", expected);
            assert!(!p.required(), "{} should not be required", expected);
        }
    }
}
