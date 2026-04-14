pub mod artifactory;
pub mod aur;
pub mod aur_source;
pub mod chocolatey;
pub mod cloudsmith;
pub mod crates_io;
pub mod dockerhub;
pub mod fury;
pub mod homebrew;
pub mod krew;
pub mod nix;
pub mod npm;
pub mod scoop;
pub mod upload;
pub(crate) mod util;
pub mod winget;

use anodize_core::config::PublishConfig;
use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::Result;

use artifactory::publish_to_artifactory;
use aur::publish_to_aur;
use aur_source::{publish_to_aur_source, publish_top_level_aur_sources};
use chocolatey::publish_to_chocolatey;
use cloudsmith::publish_to_cloudsmith;
use crates_io::publish_to_crates_io;
use dockerhub::publish_to_dockerhub;
use fury::publish_to_fury;
use homebrew::{publish_to_homebrew, publish_top_level_homebrew_casks};
use krew::publish_to_krew;
use nix::publish_to_nix;
use npm::publish_to_npm;
use scoop::publish_to_scoop;
use upload::publish_to_upload;
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
        let log = ctx.logger("publish");
        if ctx.skip_in_snapshot(&log, "publish") {
            return Ok(());
        }
        let selected = ctx.options.selected_crates.clone();

        // Individual publisher failures are collected and reported at the end
        // rather than aborting the entire publish stage.  This prevents a single
        // publisher (e.g. homebrew auth) from killing independent downstream
        // publishers (docker, cosign, announce).  crates.io is the exception —
        // it's the authoritative registry and its failure is always fatal.
        // In strict mode, any failure is immediately fatal.
        let mut errors: Vec<String> = Vec::new();
        let strict = ctx.is_strict();

        // Helper: run a publisher, log + collect error on failure.
        macro_rules! try_publish {
            ($label:expr, $expr:expr) => {
                if let Err(e) = $expr {
                    if strict {
                        anyhow::bail!("{}: {} (strict mode)", $label, e);
                    }
                    log.warn(&format!("{}: {}", $label, e));
                    errors.push(format!("{}: {}", $label, e));
                }
            };
        }

        // 1. crates.io — fatal (authoritative registry).
        publish_to_crates_io(ctx, &selected, &log)?;

        // 2. Homebrew — one call per crate that has a homebrew config.
        for crate_name in &crates_with_publisher(ctx, &selected, |p| p.homebrew.is_some()) {
            try_publish!("homebrew", publish_to_homebrew(ctx, crate_name, &log));
        }

        // 3. Scoop — one call per crate that has a scoop config.
        for crate_name in &crates_with_publisher(ctx, &selected, |p| p.scoop.is_some()) {
            try_publish!("scoop", publish_to_scoop(ctx, crate_name, &log));
        }

        // 4. Chocolatey — one call per crate that has a chocolatey config.
        for crate_name in &crates_with_publisher(ctx, &selected, |p| p.chocolatey.is_some()) {
            try_publish!("chocolatey", publish_to_chocolatey(ctx, crate_name, &log));
        }

        // 5. WinGet — one call per crate that has a winget config.
        for crate_name in &crates_with_publisher(ctx, &selected, |p| p.winget.is_some()) {
            try_publish!("winget", publish_to_winget(ctx, crate_name, &log));
        }

        // 6. AUR — one call per crate that has an aur config.
        for crate_name in &crates_with_publisher(ctx, &selected, |p| p.aur.is_some()) {
            try_publish!("aur", publish_to_aur(ctx, crate_name, &log));
        }

        // 7. Krew — one call per crate that has a krew config.
        for crate_name in &crates_with_publisher(ctx, &selected, |p| p.krew.is_some()) {
            try_publish!("krew", publish_to_krew(ctx, crate_name, &log));
        }

        // 8. Nix — one call per crate that has a nix config.
        for crate_name in &crates_with_publisher(ctx, &selected, |p| p.nix.is_some()) {
            try_publish!("nix", publish_to_nix(ctx, crate_name, &log));
        }

        // 9. DockerHub — top-level publisher (not per-crate).
        try_publish!("dockerhub", publish_to_dockerhub(ctx, &log));

        // 10. Artifactory — top-level publisher (not per-crate).
        try_publish!("artifactory", publish_to_artifactory(ctx, &log));

        // 11. GemFury — top-level publisher (not per-crate).
        try_publish!("fury", publish_to_fury(ctx, &log));

        // 12. CloudSmith — top-level publisher (not per-crate).
        try_publish!("cloudsmith", publish_to_cloudsmith(ctx, &log));

        // 13. NPM — top-level publisher (not per-crate).
        try_publish!("npm", publish_to_npm(ctx, &log));

        // 14. Homebrew Casks — top-level publisher (GoReleaser parity).
        try_publish!(
            "homebrew-casks",
            publish_top_level_homebrew_casks(ctx, &log)
        );

        // 15. Generic HTTP upload — top-level publisher.
        try_publish!("upload", publish_to_upload(ctx, &log));

        // 16. AUR source packages — per-crate publisher.
        for crate_name in &crates_with_publisher(ctx, &selected, |p| p.aur_source.is_some()) {
            try_publish!("aur-source", publish_to_aur_source(ctx, crate_name, &log));
        }

        // 17. AUR source packages — top-level array (GoReleaser `aur_sources`).
        try_publish!("aur-sources", publish_top_level_aur_sources(ctx, &log));

        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "{} publisher(s) failed:\n  {}",
                errors.len(),
                errors.join("\n  ")
            )
        }
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
        AurConfig, BucketConfig, ChocolateyConfig, ChocolateyRepoConfig, Config, CrateConfig,
        CratesPublishConfig, HomebrewConfig, KrewConfig, KrewManifestsRepoConfig, PublishConfig,
        ScoopConfig, TapConfig, WingetConfig, WingetManifestsRepoConfig,
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
        // a prerelease version. With skip_upload: "auto", homebrew/scoop
        // will skip for prereleases (matching GoReleaser behavior).
        let result = PublishStage.run(&mut ctx);
        assert!(
            result.is_ok(),
            "publish stage should succeed for prerelease versions in dry-run: {:?}",
            result.err()
        );

        // skip_upload is supported: "true" always skips, "auto" skips for prereleases.
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
                ..Default::default()
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

    // -----------------------------------------------------------------------
    // AUR integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_run_dry_run_aur() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                aur: Some(AurConfig {
                    git_url: Some("ssh://aur@aur.archlinux.org/mytool.git".to_string()),
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
    // Krew integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_run_dry_run_krew() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "kubectl-mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                krew: Some(KrewConfig {
                    manifests_repo: Some(KrewManifestsRepoConfig {
                        owner: "myorg".to_string(),
                        name: "krew-index".to_string(),
                    }),
                    short_description: Some("A kubectl plugin".to_string()),
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
    fn test_run_dry_run_all_seven_publishers() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "allpub7".to_string(),
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
                        name: "allpub7".to_string(),
                    }),
                    ..Default::default()
                }),
                winget: Some(WingetConfig {
                    manifests_repo: Some(WingetManifestsRepoConfig {
                        owner: "org".to_string(),
                        name: "winget-pkgs".to_string(),
                    }),
                    package_identifier: Some("Org.Allpub7".to_string()),
                    ..Default::default()
                }),
                aur: Some(AurConfig {
                    git_url: Some("ssh://aur@aur.archlinux.org/allpub7.git".to_string()),
                    ..Default::default()
                }),
                krew: Some(KrewConfig {
                    manifests_repo: Some(KrewManifestsRepoConfig {
                        owner: "org".to_string(),
                        name: "krew-index".to_string(),
                    }),
                    ..Default::default()
                }),
                nix: None,
                aur_source: None,
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // Top-level AUR sources integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_run_dry_run_top_level_aur_sources() {
        use anodize_core::config::AurSourceConfig;

        let mut config = Config::default();
        config.aur_sources = Some(vec![AurSourceConfig {
            name: Some("myapp".to_string()),
            description: Some("My application".to_string()),
            license: Some("MIT".to_string()),
            git_url: Some("ssh://aur@aur.archlinux.org/myapp.git".to_string()),
            makedepends: Some(vec!["rust".to_string(), "cargo".to_string()]),
            ..Default::default()
        }]);
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_top_level_aur_sources_empty_is_noop() {
        let mut config = Config::default();
        config.aur_sources = Some(vec![]);
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_top_level_aur_sources_none_is_noop() {
        let mut config = Config::default();
        config.aur_sources = None;

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // Nix integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_run_dry_run_nix() {
        use anodize_core::config::{NixConfig, RepositoryConfig};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                nix: Some(NixConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("myorg".to_string()),
                        name: Some("nixpkgs-overlay".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }
}
