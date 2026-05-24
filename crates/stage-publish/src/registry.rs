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
/// (`dockerhub`, `artifactories`, `cloudsmiths`) and instantiates a
/// `Box<dyn Publisher>` for each configured publisher. The returned slice
/// is the single source of truth that [`crate::dispatch::dispatch`]
/// iterates.
///
/// These publishers run via the trait registry. Blob and Snapcraft do NOT
/// — they own their own pipeline stages (`BlobStage`,
/// `SnapcraftPublishStage`) and record their outcomes directly into
/// `ctx.publish_report`. Registering trait-based wrappers here would fire
/// the underlying upload (`object_store::put` for blob, `snapcraft upload`
/// for snapcraft) a second time per release. See
/// `crates/stage-blob/src/run.rs::record_blob_result` and
/// `crates/stage-snapcraft/src/publish_stage.rs::record_snapcraft_result`
/// for the precedent.
///
/// The `BlobPublisher` trait impl in `stage-blob` stays for forward-compat
/// (and as a vocabulary entry the dispatch path can consult once the
/// publisher dispatch path can replace the dedicated stage entirely).
pub fn configured_publishers(ctx: &Context) -> Vec<Box<dyn Publisher>> {
    let mut v: Vec<Box<dyn Publisher>> = Vec::new();
    if is_cargo_configured(ctx) {
        v.push(Box::new(crate::cargo::CargoPublisher::new()));
    }
    // Assets group: dockerhub, artifactory, cloudsmith.
    // `blob` is also Assets-group but runs as its own `BlobStage` (see
    // doc on `configured_publishers` above for why it's not registered).
    if is_dockerhub_configured(ctx) {
        v.push(Box::new(crate::dockerhub::DockerhubPublisher::new()));
    }
    if is_artifactory_configured(ctx) {
        v.push(Box::new(crate::artifactory::ArtifactoryPublisher::new()));
    }
    if is_cloudsmith_configured(ctx) {
        v.push(Box::new(crate::cloudsmith::CloudsmithPublisher::new()));
    }
    if is_github_release_configured(ctx) {
        v.push(Box::new(
            anodizer_stage_release::publisher::GithubReleasePublisher::new(),
        ));
    }
    // Manager group — git-revert rollback against publisher-owned repo.
    if is_homebrew_configured(ctx) {
        v.push(Box::new(
            crate::homebrew::publisher::HomebrewPublisher::new(),
        ));
    }
    if is_scoop_configured(ctx) {
        v.push(Box::new(crate::scoop::ScoopPublisher::new()));
    }
    if is_nix_configured(ctx) {
        v.push(Box::new(crate::nix::publisher::NixPublisher::new()));
    }
    if is_aur_configured(ctx) {
        v.push(Box::new(crate::aur::AurOurPublisher::new()));
    }
    // Manager group — close-PR / registry rollback.
    if is_krew_configured(ctx) {
        v.push(Box::new(crate::krew::KrewPublisher::new()));
    }
    if is_mcp_configured(ctx) {
        v.push(Box::new(crate::mcp::publisher::McpPublisher::new()));
    }
    // Submitter group (no programmatic rollback — warn-only).
    if is_chocolatey_configured(ctx) {
        v.push(Box::new(crate::chocolatey::ChocolateyPublisher::new()));
    }
    if is_winget_configured(ctx) {
        v.push(Box::new(crate::winget::WingetPublisher::new()));
    }
    if crate::aur_source::is_aur_source_configured(ctx) {
        v.push(Box::new(crate::aur_source::AurSourcePublisher::new()));
    }
    // Snapcraft is intentionally NOT registered here — see the
    // doc comment on `configured_publishers` above.
    // `SnapcraftPublishStage` writes its own `PublisherResult`.
    v
}

/// True when at least one crate has a `publish.chocolatey` block.
fn is_chocolatey_configured(ctx: &Context) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.chocolatey.is_some()))
}

/// True when at least one crate has a `publish.winget` block.
fn is_winget_configured(ctx: &Context) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.winget.is_some()))
}

/// True when ANY crate has `publish.homebrew` OR the top-level
/// `homebrew_casks:` block is non-empty. Mirrors the dispatch in
/// `lib.rs` so the publisher runs whenever the existing per_crate +
/// top_level macros would have.
fn is_homebrew_configured(ctx: &Context) -> bool {
    let per_crate = ctx
        .config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.homebrew.is_some()));
    let top_level = ctx
        .config
        .homebrew_casks
        .as_ref()
        .is_some_and(|v| !v.is_empty());
    per_crate || top_level
}

/// True when at least one crate has a `publish.scoop` block.
fn is_scoop_configured(ctx: &Context) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.scoop.is_some()))
}

/// True when at least one crate has a `publish.nix` block.
fn is_nix_configured(ctx: &Context) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.nix.is_some()))
}

/// True when at least one crate has a `publish.aur` block. The
/// `publish.aur_source` upstream-AUR publisher is intentionally NOT
/// gated by this predicate — it has its own Submitter-group
/// publisher (see [`crate::aur_source::AurSourcePublisher`] +
/// [`crate::aur_source::is_aur_source_configured`]).
fn is_aur_configured(ctx: &Context) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.aur.is_some()))
}

/// True when at least one crate has a `publish.krew` block.
fn is_krew_configured(ctx: &Context) -> bool {
    ctx.config
        .crates
        .iter()
        .any(|c| c.publish.as_ref().is_some_and(|p| p.krew.is_some()))
}

/// True when the top-level `mcp.name` is set and non-empty. Mirrors
/// the skip-gate in [`crate::mcp::publish_to_mcp`] — an empty / unset
/// name short-circuits the publisher to a no-op, so we treat the same
/// state as not-configured here.
fn is_mcp_configured(ctx: &Context) -> bool {
    ctx.config
        .mcp
        .name
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty())
}

/// True when at least one crate in the active config has a
/// `publish.cargo` block. Presence of the block is the opt-in; the
/// per-crate `skip:` template is evaluated later in
/// [`crate::cargo::publish_to_cargo`].
///
/// Shape note: per-crate predicates use `.is_some()` because the inner
/// `CargoPublishConfig` is itself the opt-in — there is no list to count
/// non-empty. Top-level publishers (dockerhub, artifactories,
/// cloudsmiths) instead go through
/// [`crate::publisher_helpers::is_top_level_block_configured`], which
/// folds `Option<Vec<_>>` into a single uniform shape.
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
    crate::publisher_helpers::is_top_level_block_configured(ctx.config.dockerhub.as_ref())
}

/// True when the top-level `artifactories:` block has at least one entry.
fn is_artifactory_configured(ctx: &Context) -> bool {
    crate::publisher_helpers::is_top_level_block_configured(ctx.config.artifactories.as_ref())
}

/// True when the top-level `cloudsmiths:` block has at least one entry.
fn is_cloudsmith_configured(ctx: &Context) -> bool {
    crate::publisher_helpers::is_top_level_block_configured(ctx.config.cloudsmiths.as_ref())
}

/// True when the resolved SCM is GitHub and at least one selected
/// crate has a `release:` block configured. Mirrors the per-crate
/// filter `ReleaseStage::run` applies internally (`c.release.is_some()`)
/// so the publisher iterates the same crate universe.
///
/// GitLab and Gitea backends have their own publishers (added in a
/// follow-up task); when `ctx.token_type` is one of those,
/// [`GithubReleasePublisher`](anodizer_stage_release::publisher::GithubReleasePublisher)
/// must NOT register so the registry doesn't double-publish a single
/// release run.
fn is_github_release_configured(ctx: &Context) -> bool {
    if !matches!(ctx.token_type, anodizer_core::scm::ScmTokenType::GitHub) {
        return false;
    }
    let selected = &ctx.options.selected_crates;
    ctx.config
        .crates
        .iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .any(|c| c.release.is_some())
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
        // Every Assets-group publisher that registers in this list
        // must appear; blob is Assets-group but runs as its own
        // `BlobStage`, not via the publisher dispatch path, so it is
        // NOT registered here (asserted separately below).
        for expected in ["dockerhub", "artifactory", "cloudsmith"] {
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
        // Pin: BlobPublisher must NOT register from the stage-publish
        // registry. `BlobStage` is the load-bearing runner and writes
        // its own entry into `ctx.publish_report`; registering the
        // publisher here would double-publish every blob target.
        assert!(
            !names.contains(&"blob"),
            "blob must NOT be in the publisher registry (BlobStage owns the upload); got {:?}",
            names
        );
    }

    #[test]
    fn git_revert_publishers_registered_when_configured() {
        use anodizer_core::config::{
            AurConfig, HomebrewConfig, NixConfig, RepositoryConfig, ScoopConfig,
        };
        // Build a single crate with all four git-revert per-crate
        // publishers configured so one fixture exercises every
        // gate in `configured_publishers`.
        let demo = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                homebrew: Some(HomebrewConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("homebrew-tap".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                scoop: Some(ScoopConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("scoop-bucket".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                nix: Some(NixConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("nixpkgs-overlay".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                aur: Some(AurConfig {
                    git_url: Some("ssh://aur@aur.archlinux.org/demo-bin.git".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![demo]).build();
        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        for expected in ["homebrew", "scoop", "nix", "aur"] {
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
            assert_eq!(
                p.group(),
                PublisherGroup::Manager,
                "{} should be Manager group",
                expected
            );
            assert!(!p.required(), "{} should not be required", expected);
        }
    }

    #[test]
    fn bundle_c_publishers_registered_when_configured() {
        use anodizer_core::config::{KrewConfig, McpConfig, RepositoryConfig};
        // krew is per-crate (publish.krew); mcp is top-level (Config.mcp).
        // One fixture exercises both registration gates.
        let demo = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                krew: Some(KrewConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("krew-index-fork".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new().crates(vec![demo]).build();
        ctx.config.mcp = McpConfig {
            name: Some("io.github.acme/widget".to_string()),
            ..Default::default()
        };
        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        for expected in ["krew", "mcp"] {
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
            assert_eq!(
                p.group(),
                PublisherGroup::Manager,
                "{} should be Manager group",
                expected
            );
            assert!(!p.required(), "{} should not be required", expected);
            // krew opens a PR (rollback closes it via pull_request:write).
            // mcp posts to a registry API (no PR; rollback re-publish path
            // reads MCP_GITHUB_TOKEN — see McpPublisher rustdoc).
            let expected_scope = match expected {
                "krew" => Some("GITHUB_TOKEN pull_request:write"),
                "mcp" => Some("MCP_GITHUB_TOKEN status-mutation"),
                other => panic!("unexpected publisher in fixture: {}", other),
            };
            assert_eq!(
                p.rollback_scope_needed(),
                expected_scope,
                "{} rollback scope",
                expected
            );
        }
    }

    #[test]
    fn github_release_publisher_registered_when_configured() {
        use anodizer_core::config::{ReleaseConfig, ScmRepoConfig};
        // Per-crate `release.github` opts in. The default token_type
        // for `Context::test_fixture` / TestContextBuilder is GitHub,
        // matching the production default in `Context::new`.
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        assert!(
            names.contains(&"github-release"),
            "github-release missing from registered publishers (got {names:?})"
        );
        let p = publishers
            .iter()
            .find(|p| p.name() == "github-release")
            .expect("github-release present");
        assert_eq!(p.group(), PublisherGroup::Assets);
        assert!(p.required(), "github-release is required");
        assert_eq!(
            p.rollback_scope_needed(),
            Some("GITHUB_TOKEN contents:write")
        );
    }

    #[test]
    fn github_release_publisher_not_registered_when_scm_is_gitlab() {
        use anodizer_core::config::{ReleaseConfig, ScmRepoConfig};
        use anodizer_core::scm::ScmTokenType;
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            release: Some(ReleaseConfig {
                gitlab: Some(ScmRepoConfig {
                    owner: "acme".to_string(),
                    name: "widget".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        ctx.token_type = ScmTokenType::GitLab;
        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        assert!(
            !names.contains(&"github-release"),
            "github-release should NOT register when SCM is GitLab (got {names:?})"
        );
    }

    #[test]
    fn mcp_publisher_skipped_when_name_empty() {
        // mcp's skip-gate triggers on empty `name`. The registry
        // predicate mirrors that gate so we don't instantiate a
        // publisher whose run() would no-op anyway.
        let mut ctx = Context::test_fixture();
        ctx.config.mcp = anodizer_core::config::McpConfig {
            name: Some("   ".to_string()),
            ..Default::default()
        };
        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        assert!(
            !names.contains(&"mcp"),
            "mcp should not register when name trims to empty (got {:?})",
            names
        );
    }

    #[test]
    fn submitter_solo_publishers_registered_when_configured() {
        use anodizer_core::config::{
            AurSourceConfig, ChocolateyConfig, RepositoryConfig, WingetConfig,
        };
        // One fixture exercises all three Submitter-group "solo"
        // (no-rollback) publishers: chocolatey, winget, upstream-aur.
        // cargo is also Submitter group but lives outside this trio
        // (it has its own scope + required=true classification).
        let demo = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                chocolatey: Some(ChocolateyConfig {
                    name: Some("demo".to_string()),
                    ..Default::default()
                }),
                winget: Some(WingetConfig {
                    publisher: Some("AcmeCo".to_string()),
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("winget-pkgs-fork".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                aur_source: Some(AurSourceConfig {
                    git_url: Some("ssh://aur@aur.archlinux.org/demo.git".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![demo]).build();
        let publishers = configured_publishers(&ctx);
        let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
        let expected_scopes: &[(&str, Option<&str>)] = &[
            ("chocolatey", None),
            ("winget", Some("GITHUB_TOKEN pull_request:write")),
            ("upstream-aur", Some("AUR_SSH_KEY write")),
        ];
        for (publisher_name, expected_scope) in expected_scopes {
            assert!(
                names.contains(publisher_name),
                "{} missing from registered publishers (got {:?})",
                publisher_name,
                names
            );
            let p = publishers
                .iter()
                .find(|p| &p.name() == publisher_name)
                .expect("publisher present");
            assert_eq!(
                p.group(),
                PublisherGroup::Submitter,
                "{} should be Submitter group",
                publisher_name
            );
            assert!(!p.required(), "{} should not be required", publisher_name);
            assert_eq!(
                p.rollback_scope_needed(),
                *expected_scope,
                "{} rollback scope",
                publisher_name
            );
        }
    }

    #[test]
    fn snapcraft_unconditionally_unregistered_regardless_of_publish_flag() {
        // Pin: SnapcraftPublisher must NOT register from the
        // stage-publish registry under any `publish:` flag value.
        // `SnapcraftPublishStage` is the load-bearing runner and writes
        // its own entry into `ctx.publish_report`; a trait-based
        // wrapper here would double-publish every snap target (parallel
        // to the BlobPublisher fix in commit 026c854). The
        // table form pins ALL three input shapes (unset, false, true)
        // so a future regression that re-introduces a `publish:
        // true`-gated registration is caught.
        use anodizer_core::config::SnapcraftConfig;
        for publish_flag in [None, Some(false), Some(true)] {
            let demo = CrateConfig {
                name: "demo".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                snapcrafts: Some(vec![SnapcraftConfig {
                    name: Some("demo".to_string()),
                    publish: publish_flag,
                    channel_templates: Some(vec!["stable".to_string()]),
                    ..Default::default()
                }]),
                ..Default::default()
            };
            let ctx = TestContextBuilder::new().crates(vec![demo]).build();
            let publishers = configured_publishers(&ctx);
            let names: Vec<&str> = publishers.iter().map(|p| p.name()).collect();
            assert!(
                !names.contains(&"snapcraft"),
                "snapcraft must NOT register for publish={publish_flag:?}; got {names:?}"
            );
        }
    }
}
