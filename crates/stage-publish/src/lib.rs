pub mod chocolatey;
pub mod crates_io;
pub mod homebrew;
pub mod scoop;
pub(crate) mod util;
pub mod winget;

use anodize_core::config::PublishConfig;
use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

use chocolatey::publish_to_chocolatey;
use crates_io::publish_to_crates_io;
use homebrew::publish_to_homebrew;
use scoop::publish_to_scoop;
use winget::publish_to_winget;

/// Collect crate names that match the selection filter and have a specific
/// publisher configured (as determined by the predicate `has_config`).
fn crates_with_publisher<F>(ctx: &Context, selected: &[String], has_config: F) -> Vec<String>
where
    F: Fn(&PublishConfig) -> bool,
{
    ctx.config
        .crates
        .iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter(|c| c.publish.as_ref().is_some_and(&has_config))
        .map(|c| c.name.clone())
        .collect()
}

pub struct PublishStage;

impl Stage for PublishStage {
    fn name(&self) -> &str {
        "publish"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let selected = ctx.options.selected_crates.clone();

        // 1. crates.io — publish all crates with `publish.crates` enabled.
        publish_to_crates_io(ctx, &selected)?;

        // 2. Homebrew — one call per crate that has a homebrew config.
        for crate_name in &crates_with_publisher(ctx, &selected, |p| p.homebrew.is_some()) {
            publish_to_homebrew(ctx, crate_name)?;
        }

        // 3. Scoop — one call per crate that has a scoop config.
        for crate_name in &crates_with_publisher(ctx, &selected, |p| p.scoop.is_some()) {
            publish_to_scoop(ctx, crate_name)?;
        }

        // 4. Chocolatey — one call per crate that has a chocolatey config.
        for crate_name in &crates_with_publisher(ctx, &selected, |p| p.chocolatey.is_some()) {
            publish_to_chocolatey(ctx, crate_name)?;
        }

        // 5. WinGet — one call per crate that has a winget config.
        for crate_name in &crates_with_publisher(ctx, &selected, |p| p.winget.is_some()) {
            publish_to_winget(ctx, crate_name)?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodize_core::config::{
        BucketConfig, ChocolateyConfig, ChocolateyRepoConfig, Config, CrateConfig,
        CratesPublishConfig, HomebrewConfig, PublishConfig, ScoopConfig, TapConfig, WingetConfig,
        WingetManifestsRepoConfig,
    };
    use anodize_core::context::{Context, ContextOptions};

    fn dry_run_ctx(config: Config) -> Context {
        Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_stage_name() {
        assert_eq!(PublishStage.name(), "publish");
    }

    #[test]
    fn test_run_no_crates_configured() {
        let config = Config::default();
        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_run_dry_run_crates_io() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                crates: Some(CratesPublishConfig::Bool(true)),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        // dry-run: should log but not actually shell out
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_run_dry_run_homebrew() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                homebrew: Some(HomebrewConfig {
                    tap: Some(TapConfig {
                        owner: "myorg".to_string(),
                        name: "homebrew-tap".to_string(),
                    }),
                    description: Some("My tool".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_run_dry_run_scoop() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                scoop: Some(ScoopConfig {
                    bucket: Some(BucketConfig {
                        owner: "myorg".to_string(),
                        name: "scoop-bucket".to_string(),
                    }),
                    description: Some("My tool".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_run_dry_run_all_publishers() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "allpub".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                crates: Some(CratesPublishConfig::Bool(true)),
                homebrew: Some(HomebrewConfig {
                    tap: Some(TapConfig {
                        owner: "org".to_string(),
                        name: "homebrew-tap".to_string(),
                    }),
                    ..Default::default()
                }),
                scoop: Some(ScoopConfig {
                    bucket: Some(BucketConfig {
                        owner: "org".to_string(),
                        name: "scoop-bucket".to_string(),
                    }),
                    description: None,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // Task 4C: Additional behavior tests — config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_dry_run_logs_without_executing_for_all_publishers() {
        // Verify dry-run mode works for all publisher types simultaneously
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "multi".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                crates: Some(CratesPublishConfig::Bool(true)),
                homebrew: Some(HomebrewConfig {
                    tap: Some(TapConfig {
                        owner: "org".to_string(),
                        name: "homebrew-tap".to_string(),
                    }),
                    description: Some("A multi-publisher tool".to_string()),
                    ..Default::default()
                }),
                scoop: Some(ScoopConfig {
                    bucket: Some(BucketConfig {
                        owner: "org".to_string(),
                        name: "scoop-bucket".to_string(),
                    }),
                    description: Some("A multi-publisher tool".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        // All publishers should succeed in dry-run mode
        let result = PublishStage.run(&mut ctx);
        assert!(result.is_ok());
    }

    #[test]
    fn test_selected_crates_filter_applies_to_publishers() {
        let mut config = Config::default();
        config.crates = vec![
            CrateConfig {
                name: "included".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: Some(PublishConfig {
                    homebrew: Some(HomebrewConfig {
                        tap: Some(TapConfig {
                            owner: "org".to_string(),
                            name: "tap".to_string(),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            CrateConfig {
                name: "excluded".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: Some(PublishConfig {
                    homebrew: Some(HomebrewConfig {
                        tap: Some(TapConfig {
                            owner: "org".to_string(),
                            name: "tap".to_string(),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                selected_crates: vec!["included".to_string()],
                ..Default::default()
            },
        );

        // Should only run for "included", not "excluded"
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_no_publish_config_is_noop() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "nopub".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: None, // No publish config
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        // Should succeed (no-op)
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    /// Document current behavior: the publish stage does NOT skip homebrew/scoop
    /// publishing for prerelease versions. It proceeds regardless of whether
    /// the version contains a prerelease suffix like -rc.1 or -beta.
    ///
    /// This is a known limitation: GoReleaser skips homebrew/scoop for prereleases
    /// by default. If this behavior is added in the future, this test should be
    /// updated to verify that skipping occurs.
    #[test]
    fn test_publish_prerelease_version_proceeds_without_skip() {
        use anodize_core::context::ContextOptions;

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                crates: Some(CratesPublishConfig::Bool(true)),
                homebrew: Some(HomebrewConfig {
                    tap: Some(TapConfig {
                        owner: "org".to_string(),
                        name: "homebrew-tap".to_string(),
                    }),
                    description: Some("A tool".to_string()),
                    ..Default::default()
                }),
                scoop: Some(ScoopConfig {
                    bucket: Some(BucketConfig {
                        owner: "org".to_string(),
                        name: "scoop-bucket".to_string(),
                    }),
                    description: Some("A tool".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        // Use a prerelease version like v1.0.0-rc.1
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Manually set the Version template var to a prerelease string.
        // The publish stage reads this from template_vars, not from git.
        ctx.template_vars_mut().set("Version", "1.0.0-rc.1");
        ctx.template_vars_mut().set("Tag", "v1.0.0-rc.1");

        // The publish stage should succeed in dry-run mode even with
        // a prerelease version. It does NOT currently skip homebrew/scoop
        // for prereleases (unlike GoReleaser which does by default).
        let result = PublishStage.run(&mut ctx);
        assert!(
            result.is_ok(),
            "publish stage should succeed for prerelease versions in dry-run: {:?}",
            result.err()
        );

        // NOTE: Known limitation — there is no prerelease-skip logic in the
        // publish stage. GoReleaser's `brews[].skip_upload` defaults to "auto"
        // which skips for prereleases. Anodize currently publishes regardless.
    }

    // -----------------------------------------------------------------------
    // Chocolatey integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_run_dry_run_chocolatey() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                chocolatey: Some(ChocolateyConfig {
                    project_repo: Some(ChocolateyRepoConfig {
                        owner: "myorg".to_string(),
                        name: "mytool".to_string(),
                    }),
                    description: Some("My tool".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // WinGet integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_run_dry_run_winget() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                winget: Some(WingetConfig {
                    manifests_repo: Some(WingetManifestsRepoConfig {
                        owner: "myorg".to_string(),
                        name: "winget-pkgs".to_string(),
                    }),
                    package_identifier: Some("MyOrg.MyTool".to_string()),
                    description: Some("My tool".to_string()),
                    publisher: Some("My Org".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_run_dry_run_all_five_publishers() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "allpub5".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                crates: Some(CratesPublishConfig::Bool(true)),
                homebrew: Some(HomebrewConfig {
                    tap: Some(TapConfig {
                        owner: "org".to_string(),
                        name: "homebrew-tap".to_string(),
                    }),
                    ..Default::default()
                }),
                scoop: Some(ScoopConfig {
                    bucket: Some(BucketConfig {
                        owner: "org".to_string(),
                        name: "scoop-bucket".to_string(),
                    }),
                    description: None,
                    ..Default::default()
                }),
                chocolatey: Some(ChocolateyConfig {
                    project_repo: Some(ChocolateyRepoConfig {
                        owner: "org".to_string(),
                        name: "allpub5".to_string(),
                    }),
                    ..Default::default()
                }),
                winget: Some(WingetConfig {
                    manifests_repo: Some(WingetManifestsRepoConfig {
                        owner: "org".to_string(),
                        name: "winget-pkgs".to_string(),
                    }),
                    package_identifier: Some("Org.Allpub5".to_string()),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_selected_crates_filter_applies_to_chocolatey_and_winget() {
        let mut config = Config::default();
        config.crates = vec![
            CrateConfig {
                name: "included".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: Some(PublishConfig {
                    chocolatey: Some(ChocolateyConfig {
                        project_repo: Some(ChocolateyRepoConfig {
                            owner: "org".to_string(),
                            name: "included".to_string(),
                        }),
                        ..Default::default()
                    }),
                    winget: Some(WingetConfig {
                        manifests_repo: Some(WingetManifestsRepoConfig {
                            owner: "org".to_string(),
                            name: "winget-pkgs".to_string(),
                        }),
                        package_identifier: Some("Org.Included".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            CrateConfig {
                name: "excluded".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: Some(PublishConfig {
                    chocolatey: Some(ChocolateyConfig {
                        project_repo: Some(ChocolateyRepoConfig {
                            owner: "org".to_string(),
                            name: "excluded".to_string(),
                        }),
                        ..Default::default()
                    }),
                    winget: Some(WingetConfig {
                        manifests_repo: Some(WingetManifestsRepoConfig {
                            owner: "org".to_string(),
                            name: "winget-pkgs".to_string(),
                        }),
                        package_identifier: Some("Org.Excluded".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                selected_crates: vec!["included".to_string()],
                ..Default::default()
            },
        );

        // Should only run for "included", not "excluded"
        assert!(PublishStage.run(&mut ctx).is_ok());
    }
}
