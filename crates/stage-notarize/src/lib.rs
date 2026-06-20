//! macOS code-signing + notarization stage.
//!
//! Split into focused submodules:
//!
//! - [`secret`] — checksum refresh, skip/id gating, base64 secret
//!   materialization + arg redaction.
//! - [`retry`] — notarytool / rcodesign invocation with bounded transient
//!   retry and output checking.
//! - [`run`] — the cross-platform (rcodesign) and native (codesign + xcrun
//!   notarytool) per-config run paths.
//!
//! The [`NotarizeStage`] entry point and its [`Stage`] impl live here.

use anyhow::{Context as _, Result, bail};

use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

mod retry;
mod run;
mod secret;

use run::{run_cross_platform, run_native};
use secret::refresh_artifact_checksums;

// Exercised only by the unix-gated tests below (they fabricate an
// `ExitStatus` via the unix-only `ExitStatusExt::from_raw`, and the retry
// logic itself guards macOS-only notarization), so the import must carry the
// same `unix` gate or it reads as unused on a Windows build.
#[cfg(all(test, unix))]
use retry::{is_retriable_notarize_output, run_with_retry};
#[cfg(test)]
use secret::matches_ids;

pub struct NotarizeStage;

impl Stage for NotarizeStage {
    fn name(&self) -> &str {
        "notarize"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("notarize");
        let dry_run = ctx.options.dry_run;

        let notarize_config = match ctx.config.notarize {
            Some(ref cfg) => cfg,
            None => return Ok(()),
        };

        // Respect top-level skip flag. Use try_evaluates_to_true so a malformed
        // skip: template surfaces as Err instead of silently evaluating
        // false and running notarization the user thought was suppressed.
        if let Some(ref d) = notarize_config.skip
            && d.try_evaluates_to_true(|s| ctx.render_template(s))
                .with_context(|| "notarize: evaluate top-level skip expression")?
        {
            log.skip_line(ctx.options.show_skipped, "notarization skipped");
            return Ok(());
        }

        // `macos` and `macos_native` are mutually exclusive — they sign and
        // notarize the same artifacts via different toolchains. Refuse a
        // config that populates both so a binary doesn't get signed twice
        // (the second pass would invalidate the first signature).
        let has_cross = notarize_config
            .macos
            .as_ref()
            .is_some_and(|v| !v.is_empty());
        let has_native = notarize_config
            .macos_native
            .as_ref()
            .is_some_and(|v| !v.is_empty());
        if has_cross && has_native {
            bail!(
                "notarize: 'macos' and 'macos_native' cannot both be populated — \
                 they sign and notarize the same artifacts via different toolchains. \
                 Pick one (rcodesign for macos, codesign+notarytool for macos_native)."
            );
        }

        // Cross-platform signing/notarization (rcodesign)
        if let Some(ref macos_configs) = notarize_config.macos {
            for (idx, cfg) in macos_configs.iter().enumerate() {
                run_cross_platform(ctx, cfg, idx, dry_run, &log)?;
            }
        }

        // Native signing/notarization (codesign + xcrun notarytool)
        if let Some(ref native_configs) = notarize_config.macos_native {
            for (idx, cfg) in native_configs.iter().enumerate() {
                run_native(ctx, cfg, idx, dry_run, &log)?;
            }
        }

        // Refresh artifact checksums after signing.
        // Signing modifies binaries in-place, so SHA256 metadata becomes stale.
        if !dry_run {
            refresh_artifact_checksums(ctx, &log);
        }

        Ok(())
    }
}

/// Environment requirements for the notarize stage, mirroring its run
/// gates: nothing when the top-level `skip:` is truthy; per active
/// `macos:` entry the cross-platform `rcodesign` plus the env refs of the
/// templated certificate / password / App Store Connect fields; per active
/// `macos_native:` entry `codesign` + `xcrun` plus the env refs of the
/// templated identity / keychain / profile fields. Both toolchains run on
/// whatever host executes the release (rcodesign is cross-platform; a
/// `macos_native` config on a non-mac host would fail at run time, and
/// preflight reports exactly that). Values are never echoed — only
/// referenced env-var names.
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    use anodizer_core::env_preflight::template_env_refs;
    let Some(cfg) = ctx.config.notarize.as_ref() else {
        return Vec::new();
    };
    if anodizer_core::env_preflight::entry_inactive(ctx, cfg.skip.as_ref(), None, None) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let push_refs = |out: &mut Vec<anodizer_core::EnvRequirement>, value: Option<&str>| {
        if let Some(v) = value {
            let refs = template_env_refs(v);
            if !refs.is_empty() {
                out.push(anodizer_core::EnvRequirement::EnvAllOf { vars: refs });
            }
        }
    };
    for entry in cfg.macos.iter().flatten() {
        // Unrenderable skip counts as active: over-collect.
        if entry
            .should_skip(|s| ctx.render_template(s))
            .unwrap_or(false)
        {
            continue;
        }
        out.push(anodizer_core::EnvRequirement::Tool {
            name: "rcodesign".to_string(),
        });
        if let Some(sign) = entry.sign.as_ref() {
            push_refs(&mut out, sign.certificate.as_deref());
            push_refs(&mut out, sign.password.as_deref());
        }
        if let Some(notarize) = entry.notarize.as_ref() {
            push_refs(&mut out, notarize.issuer_id.as_deref());
            push_refs(&mut out, notarize.key.as_deref());
            push_refs(&mut out, notarize.key_id.as_deref());
        }
    }
    for entry in cfg.macos_native.iter().flatten() {
        if entry
            .should_skip(|s| ctx.render_template(s))
            .unwrap_or(false)
        {
            continue;
        }
        out.push(anodizer_core::EnvRequirement::Tool {
            name: "codesign".to_string(),
        });
        out.push(anodizer_core::EnvRequirement::Tool {
            name: "xcrun".to_string(),
        });
        if let Some(sign) = entry.sign.as_ref() {
            push_refs(&mut out, sign.identity.as_deref());
            push_refs(&mut out, sign.keychain.as_deref());
        }
        if let Some(notarize) = entry.notarize.as_ref() {
            push_refs(&mut out, notarize.profile_name.as_deref());
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        Config, MacOSNativeNotarizeConfig, MacOSNativeSignConfig, MacOSNativeSignNotarizeConfig,
        MacOSNotarizeApiConfig, MacOSSignConfig, MacOSSignNotarizeConfig, NotarizeConfig,
        StringOrBool,
    };
    use anodizer_core::context::{Context, ContextOptions};

    // -----------------------------------------------------------------------
    // Config deserialization tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_platform_config_deserializes() {
        // Per-config gating uses the canonical `skip:` field; the block
        // below opts in implicitly (no `skip:` = run).
        let yaml = r#"
notarize:
  macos:
    - ids: [myapp]
      sign:
        certificate: /path/to/cert.p12
        password: "s3cret"
        entitlements: entitlements.xml
      notarize:
        issuer_id: "abc-123"
        key: /path/to/key.p8
        key_id: "KEY123"
        timeout: "15m"
        wait: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        let macos = notarize.macos.unwrap();
        assert_eq!(macos.len(), 1);

        let entry = &macos[0];
        assert_eq!(entry.skip, None);
        assert_eq!(entry.ids, Some(vec!["myapp".to_string()]));

        let sign = entry.sign.as_ref().unwrap();
        assert_eq!(sign.certificate, Some("/path/to/cert.p12".to_string()));
        assert_eq!(sign.password, Some("s3cret".to_string()));
        assert_eq!(sign.entitlements, Some("entitlements.xml".to_string()));

        let notarize_api = entry.notarize.as_ref().unwrap();
        assert_eq!(notarize_api.issuer_id, Some("abc-123".to_string()));
        assert_eq!(notarize_api.key, Some("/path/to/key.p8".to_string()));
        assert_eq!(notarize_api.key_id, Some("KEY123".to_string()));
        assert_eq!(
            notarize_api.timeout.map(|d| d.as_humantime_string()),
            Some("15m".to_string())
        );
        assert_eq!(notarize_api.wait, Some(true));
    }

    #[test]
    fn test_native_config_deserializes() {
        let yaml = r#"
notarize:
  macos_native:
    - use: dmg
      ids: [myapp]
      sign:
        identity: "Developer ID Application: Example"
        keychain: /path/to/keychain
        options: [runtime]
        entitlements: entitlements.xml
      notarize:
        profile_name: "my-profile"
        wait: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        let native = notarize.macos_native.unwrap();
        assert_eq!(native.len(), 1);

        let entry = &native[0];
        assert_eq!(entry.skip, None);
        assert_eq!(
            entry.use_,
            Some(anodizer_core::config::MacOSNativeArtifactKind::Dmg)
        );
        assert_eq!(entry.ids, Some(vec!["myapp".to_string()]));

        let sign = entry.sign.as_ref().unwrap();
        assert_eq!(
            sign.identity,
            Some("Developer ID Application: Example".to_string())
        );
        assert_eq!(sign.keychain, Some("/path/to/keychain".to_string()));
        assert_eq!(sign.options, Some(vec!["runtime".to_string()]));
        assert_eq!(sign.entitlements, Some("entitlements.xml".to_string()));

        let notarize_cfg = entry.notarize.as_ref().unwrap();
        assert_eq!(notarize_cfg.profile_name, Some("my-profile".to_string()));
        assert_eq!(notarize_cfg.wait, Some(true));
    }

    #[test]
    fn test_native_config_pkg_mode_deserializes() {
        let yaml = r#"
notarize:
  macos_native:
    - use: pkg
      sign:
        identity: "Developer ID Installer: Example"
      notarize:
        profile_name: "my-profile"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        let native = notarize.macos_native.unwrap();
        assert_eq!(
            native[0].use_,
            Some(anodizer_core::config::MacOSNativeArtifactKind::Pkg)
        );
    }

    #[test]
    fn test_skip_string_template_deserializes() {
        // The template form of `skip:` still parses on per-config
        // notarize blocks.
        let yaml = r#"
notarize:
  macos:
    - skip: "{{ if .IsSnapshot }}true{{ endif }}"
      sign:
        certificate: cert.p12
        password: pass
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let macos = config.notarize.unwrap().macos.unwrap();
        match &macos[0].skip {
            Some(StringOrBool::String(s)) => {
                assert_eq!(s, "{{ if .IsSnapshot }}true{{ endif }}")
            }
            other => panic!("expected StringOrBool::String, got {:?}", other),
        }
    }

    #[test]
    fn test_both_modes_in_single_config() {
        let yaml = r#"
notarize:
  macos:
    - sign:
        certificate: cert.p12
        password: pass
  macos_native:
    - sign:
        identity: "Developer ID Application: Test"
      notarize:
        profile_name: test-profile
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        assert!(notarize.macos.is_some());
        assert!(notarize.macos_native.is_some());
    }

    #[test]
    fn test_empty_notarize_config_deserializes() {
        let yaml = r#"
notarize: {}
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        assert!(notarize.macos.is_none());
        assert!(notarize.macos_native.is_none());
    }

    // -----------------------------------------------------------------------
    // Stage skipping / enabled logic tests
    // -----------------------------------------------------------------------

    fn make_ctx_with_notarize(config: Config) -> Context {
        Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_stage_skips_when_no_notarize_config() {
        let config = Config::default();
        let mut ctx = make_ctx_with_notarize(config);

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
        // Should succeed with no-op
    }

    #[test]
    fn test_stage_skips_disabled_cross_platform() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
        // Should succeed without errors (disabled)
    }

    #[test]
    fn test_stage_skips_disabled_native() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_stage_skips_when_enabled_is_none() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        // Should skip because enabled defaults to false
        stage.run(&mut ctx).unwrap();
    }

    // -----------------------------------------------------------------------
    // Required field validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_platform_requires_sign_config() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: None,
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("requires a 'sign'"),
            "error should mention missing sign config"
        );
    }

    #[test]
    fn test_cross_platform_requires_certificate() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSSignConfig {
                    certificate: None,
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("sign.certificate is required"),
        );
    }

    #[test]
    fn test_cross_platform_requires_password() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: None,
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("sign.password is required"),
        );
    }

    #[test]
    fn test_native_requires_sign_config() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                sign: None,
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("requires a 'sign'"),
        );
    }

    #[test]
    fn test_native_requires_identity() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSNativeSignConfig {
                    identity: None,
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("sign.identity is required"),
        );
    }

    #[test]
    fn test_native_requires_notarize_config() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID".to_string()),
                    ..Default::default()
                }),
                notarize: None,
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("requires a 'notarize'"),
        );
    }

    #[test]
    fn test_native_requires_profile_name() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: None,
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("notarize.profile_name is required"),
        );
    }

    #[test]
    fn test_native_rejects_unsupported_use_type_at_parse_time() {
        // `notarize.macos_native.use` is a typed enum; unsupported values
        // must fail at parse time instead of producing a silent no-op.
        let yaml = r#"
notarize:
  macos_native:
    - use: zip
      sign:
        identity: "Developer ID"
      notarize:
        profile_name: "profile"
crates: []
"#;
        let result: std::result::Result<Config, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "macos_native.use: zip must be rejected (only 'dmg' / 'pkg' allowed)"
        );
    }

    // -----------------------------------------------------------------------
    // Dry-run behavior tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_platform_dry_run_with_darwin_binaries() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    entitlements: Some("ent.xml".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNotarizeApiConfig {
                    issuer_id: Some("issuer-123".to_string()),
                    key: Some("key.p8".to_string()),
                    key_id: Some("KEY1".to_string()),
                    wait: Some(true),
                    timeout: Some(anodizer_core::config::HumanDuration(
                        std::time::Duration::from_secs(20 * 60),
                    )),
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Register darwin binary artifacts
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_x86"),
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        // Also register a linux binary that should be ignored
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_linux"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        // Should succeed without actually invoking rcodesign
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_cross_platform_dry_run_sign_only_no_notarize() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                notarize: None, // sign-only
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_cross_platform_no_darwin_binaries_is_noop() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Only register Linux binaries
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_native_dmg_dry_run_with_artifacts() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                use_: Some(anodizer_core::config::MacOSNativeArtifactKind::Dmg),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Application: Test".to_string()),
                    keychain: Some("/path/to/kc".to_string()),
                    options: Some(vec!["runtime".to_string()]),
                    entitlements: Some("ent.xml".to_string()),
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    wait: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Register an app bundle artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.app"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "appbundle".to_string())]),
            size: None,
        });

        // Register a DMG artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DiskImage,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.dmg"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "dmg".to_string())]),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_native_pkg_dry_run_with_artifacts() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                use_: Some(anodizer_core::config::MacOSNativeArtifactKind::Pkg),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Installer: Test".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    wait: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Register a MacOsPackage artifact (not appbundle)
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::MacOsPackage,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.pkg"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([
                ("format".to_string(), "pkg".to_string()),
                ("identifier".to_string(), "com.example.myapp".to_string()),
            ]),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_native_dmg_no_matching_artifacts_is_noop() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                use_: Some(anodizer_core::config::MacOSNativeArtifactKind::Dmg),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Application: Test".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // No artifacts registered at all
        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_native_pkg_no_matching_artifacts_is_noop() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                use_: Some(anodizer_core::config::MacOSNativeArtifactKind::Pkg),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Installer: Test".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    // -----------------------------------------------------------------------
    // Artifact filtering tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_platform_ids_filter() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                ids: Some(vec!["other-crate".to_string()]),
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // This binary is for "myapp" but ids filter is ["other-crate"]
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        // Should succeed with no-op since id doesn't match
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_matches_ids_helper_no_filter() {
        let artifact = Artifact {
            kind: ArtifactKind::Binary,
            name: "test".to_string(),
            path: PathBuf::from("dist/test"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        };

        assert!(matches_ids(&artifact, &None));
        assert!(matches_ids(&artifact, &Some(vec![])));
    }

    #[test]
    fn test_matches_ids_helper_no_id_metadata_does_not_match() {
        let artifact = Artifact {
            kind: ArtifactKind::Binary,
            name: "test".to_string(),
            path: PathBuf::from("dist/test"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        };

        assert!(!matches_ids(&artifact, &Some(vec!["myapp".to_string()])));
        assert!(!matches_ids(&artifact, &Some(vec!["other".to_string()])));
    }

    #[test]
    fn test_matches_ids_helper_by_metadata_id() {
        let artifact = Artifact {
            kind: ArtifactKind::Binary,
            name: "test".to_string(),
            path: PathBuf::from("dist/test"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-arm".to_string())]),
            size: None,
        };

        assert!(matches_ids(&artifact, &Some(vec!["build-arm".to_string()])));
        assert!(!matches_ids(&artifact, &Some(vec!["myapp".to_string()])));
    }

    #[test]
    fn test_cross_platform_filters_non_darwin_artifacts() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Only non-darwin targets
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
        // No darwin artifacts, so this is a no-op
    }

    #[test]
    fn test_native_dmg_filters_appbundle_by_ids() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                ids: Some(vec!["other".to_string()]),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // This artifact has crate_name "myapp" but ids filter is ["other"]
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.app"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "appbundle".to_string())]),
            size: None,
        });

        let stage = NotarizeStage;
        // Should succeed as no-op since ids don't match
        stage.run(&mut ctx).unwrap();
    }

    // -----------------------------------------------------------------------
    // should_skip gating tests (per-config `skip:` / inverted `enabled:`)
    // -----------------------------------------------------------------------

    /// Build a `MacOSSignNotarizeConfig` with the given `skip` and a render
    /// context exposing `IsSnapshot`, returning the `should_skip` result.
    fn should_skip_with(skip: Option<StringOrBool>, is_snapshot: bool) -> anyhow::Result<bool> {
        let mut cfg = MacOSSignNotarizeConfig::default();
        cfg.skip = skip;
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.template_vars_mut()
            .set("IsSnapshot", if is_snapshot { "true" } else { "false" });
        cfg.should_skip(|s| ctx.render_template(s))
    }

    #[test]
    fn test_should_skip_none_runs() {
        // None -> run (default opt-in once notarize block is present).
        assert!(!should_skip_with(None, false).unwrap());
    }

    #[test]
    fn test_should_skip_bool_true_skips() {
        assert!(should_skip_with(Some(StringOrBool::Bool(true)), false).unwrap());
    }

    #[test]
    fn test_should_skip_bool_false_runs() {
        assert!(!should_skip_with(Some(StringOrBool::Bool(false)), false).unwrap());
    }

    #[test]
    fn test_should_skip_string_true_skips() {
        assert!(should_skip_with(Some(StringOrBool::String("true".into())), false).unwrap());
    }

    #[test]
    fn test_should_skip_string_false_runs() {
        assert!(!should_skip_with(Some(StringOrBool::String("false".into())), false).unwrap());
    }

    /// Build a `MacOSSignNotarizeConfig` with the given direct `skip:` value
    /// and a context where `{{ .Marker }}` renders to `marker`.
    fn should_skip_marker(skip: &str, marker: &str) -> anyhow::Result<bool> {
        let mut cfg = MacOSSignNotarizeConfig::default();
        cfg.skip = Some(StringOrBool::String(skip.into()));
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.template_vars_mut().set("Marker", marker);
        cfg.should_skip(|s| ctx.render_template(s))
    }

    #[test]
    fn test_should_skip_direct_template_uses_sibling_truthy() {
        // A DIRECT `skip:` template must use the sibling truthy convention
        // (`try_evaluates_to_true`: only "true"/"1" are truthy), NOT the wider
        // inverted-`enabled:` falsy blacklist. `1` skips; `yes`/`on` do not —
        // matching `should_skip_upload` and every other publisher gate.
        assert!(
            should_skip_marker("{{ .Marker }}", "1").unwrap(),
            "direct skip rendering '1' must skip (sibling truthy)"
        );
        assert!(
            !should_skip_marker("{{ .Marker }}", "yes").unwrap(),
            "direct skip rendering 'yes' must RUN (not a sibling-truthy value)"
        );
        assert!(
            !should_skip_marker("{{ .Marker }}", "on").unwrap(),
            "direct skip rendering 'on' must RUN (not a sibling-truthy value)"
        );
    }

    #[test]
    fn test_inverted_enabled_uses_wider_falsy_set() {
        // The inverted `enabled:` path keeps the wider falsy blacklist: a
        // non-falsy render (e.g. `yes`/`on`/`1`) means enabled → RUN; only
        // ""/false/0/no disable. Contrast with the direct `skip:` path above.
        assert!(
            !enabled_should_skip_marker("{{ .Marker }}", "yes").unwrap(),
            "enabled rendering 'yes' is truthy → run (must not skip)"
        );
        assert!(
            enabled_should_skip_marker("{{ .Marker }}", "no").unwrap(),
            "enabled rendering 'no' is falsy → skip"
        );
    }

    /// `enabled:`-alias variant of [`should_skip_marker`] — renders
    /// `{{ .Marker }}` to `marker` through the inverted-enabled path.
    fn enabled_should_skip_marker(enabled: &str, marker: &str) -> anyhow::Result<bool> {
        let yaml = format!(
            "notarize:\n  macos:\n    - enabled: \"{enabled}\"\n      sign:\n        certificate: /tmp/c.p12\n        password: pw\ncrates: []\n"
        );
        let cfg: Config = serde_yaml_ng::from_str(&yaml).expect("enabled alias should parse");
        let entry = cfg.notarize.unwrap().macos.unwrap().remove(0);
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.template_vars_mut().set("Marker", marker);
        entry.should_skip(|s| ctx.render_template(s))
    }

    #[test]
    fn test_should_skip_template_skip_truthy() {
        // A direct `skip:` template that renders truthy skips.
        let r = should_skip_with(
            Some(StringOrBool::String(
                "{{ if .IsSnapshot }}true{{ end }}".into(),
            )),
            true,
        )
        .unwrap();
        assert!(r, "skip template rendering truthy must skip");
    }

    #[test]
    fn test_should_skip_malformed_skip_template_fails_closed() {
        // A malformed `skip:` template must surface as Err (fail closed),
        // not silently evaluate false and run.
        let r = should_skip_with(Some(StringOrBool::String("{{ broken".into())), false);
        assert!(r.is_err(), "malformed skip template must error, not run");
    }

    // -----------------------------------------------------------------------
    // Inverted `enabled:` — must NOT fail open
    // -----------------------------------------------------------------------

    /// Parse a one-entry `notarize.macos` block carrying the given `enabled:`
    /// value and return its `should_skip` against a context with `IsSnapshot`.
    fn enabled_should_skip(enabled_yaml: &str, is_snapshot: bool) -> anyhow::Result<bool> {
        let yaml = format!(
            "notarize:\n  macos:\n    - enabled: {enabled_yaml}\n      sign:\n        certificate: /tmp/c.p12\n        password: pw\ncrates: []\n"
        );
        let cfg: Config = serde_yaml_ng::from_str(&yaml).expect("enabled alias should parse");
        let entry = cfg.notarize.unwrap().macos.unwrap().remove(0);
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.template_vars_mut()
            .set("IsSnapshot", if is_snapshot { "true" } else { "false" });
        entry.should_skip(|s| ctx.render_template(s))
    }

    #[test]
    fn test_enabled_literal_false_disables() {
        // `enabled: "false"` must DISABLE (skip), not run.
        assert!(
            enabled_should_skip("\"false\"", false).unwrap(),
            "enabled: false must skip"
        );
    }

    #[test]
    fn test_enabled_template_falsy_disables() {
        // `enabled: "{{ <falsy> }}"` must DISABLE. IsSnapshot=false renders
        // the expression to "false" → enabled falsy → skip.
        assert!(
            enabled_should_skip("\"{{ .IsSnapshot }}\"", false).unwrap(),
            "templated enabled rendering falsy must skip (not silently run)"
        );
    }

    #[test]
    fn test_enabled_template_truthy_runs() {
        // Same template, IsSnapshot=true → enabled truthy → run.
        assert!(
            !enabled_should_skip("\"{{ .IsSnapshot }}\"", true).unwrap(),
            "templated enabled rendering truthy must run"
        );
    }

    #[test]
    fn test_enabled_malformed_template_fails_closed() {
        // A malformed `enabled:` template must NOT silently enable. It must
        // surface as Err (the caller treats the entry as skipped / aborts) —
        // the prior `{% if {{ … }} %}` construction produced malformed Tera
        // that errored and was swallowed as "run" (fail-open safety hole).
        let r = enabled_should_skip("\"{{ broken\"", false);
        assert!(
            r.is_err(),
            "malformed enabled template must error, not silently enable notarization"
        );
    }

    // -----------------------------------------------------------------------
    // Native DMG mode defaults to "dmg" when use_ is None
    // -----------------------------------------------------------------------

    #[test]
    fn test_native_defaults_to_dmg_when_use_is_none() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                use_: None, // should default to "dmg"
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Application: Test".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Register a DMG so the stage has something to find (or not)
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DiskImage,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.dmg"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "dmg".to_string())]),
            size: None,
        });

        let stage = NotarizeStage;
        // Should succeed because it defaults to DMG mode
        stage.run(&mut ctx).unwrap();
    }

    // -----------------------------------------------------------------------
    // M6: notarize retry tests
    // -----------------------------------------------------------------------

    /// Build a synthetic `Output` with a non-zero exit and the given stderr,
    /// useful for exercising `is_retriable_notarize_output` without actually
    /// running a process. The exit status is constructed via the os-specific
    /// `from_raw` helpers so we don't need to depend on a child process.
    #[cfg(unix)]
    fn fake_output(stderr: &str, code: i32) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn test_logger() -> anodizer_core::log::StageLogger {
        anodizer_core::log::StageLogger::new("notarize", anodizer_core::log::Verbosity::Quiet)
    }

    #[cfg(unix)]
    #[test]
    fn test_is_retriable_notarize_output_network_markers() {
        // Network-side blips: must classify as retriable.
        let log = test_logger();
        for marker in [
            "tls: bad record",
            "i/o timeout",
            "could not resolve host",
            "503 service unavailable",
            "429 too many requests",
            "dial tcp: connection refused",
            "connection reset by peer",
        ] {
            let out = fake_output(marker, 1);
            assert!(
                is_retriable_notarize_output(&out, &log),
                "should retry on '{marker}'"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_is_retriable_notarize_output_apple_rejection_is_terminal() {
        // Apple-side hard rejections: must NOT retry. Re-submitting an
        // invalid bundle is wasted API quota and worse UX (multi-minute
        // delays before the user sees the real error).
        let log = test_logger();
        for marker in [
            "status: Invalid",
            "Invalid submission",
            "status: Rejected",
            "submission rejected by Apple",
        ] {
            let out = fake_output(marker, 1);
            assert!(
                !is_retriable_notarize_output(&out, &log),
                "must NOT retry on '{marker}'"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_is_retriable_notarize_output_unknown_failure_is_terminal() {
        // An exit failure with no recognised network marker (e.g. malformed
        // CLI args, certificate not found) is treated as terminal — retrying
        // will not help.
        let out = fake_output("error: --p12-file: no such file", 64);
        assert!(!is_retriable_notarize_output(&out, &test_logger()));
    }

    #[cfg(unix)]
    #[test]
    fn test_run_with_retry_returns_immediately_on_terminal_error() {
        // Drive `run_with_retry` through `false`, which exits 1 with no
        // stderr — classifies as non-retriable and should return on the
        // first attempt without invoking the delay function. A no-op delay
        // closure ensures the test cannot accidentally sleep 30s if the
        // classification logic ever drifts.
        let log = anodizer_core::log::StageLogger::new(
            "notarize-test",
            anodizer_core::log::Verbosity::Quiet,
        );
        let no_delay = |_d: std::time::Duration| {};
        let args = vec!["false".to_string()];
        let result = run_with_retry(&args, "false-cmd", &log, &no_delay).unwrap();
        assert!(!result.status.success());
    }

    #[cfg(unix)]
    #[test]
    fn test_run_status_with_retry_surfaces_child_output_only_at_verbose() {
        use crate::retry::run_status_with_retry;
        let no_delay = |_d: std::time::Duration| {};
        let args = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo NOTARYCHATTER".to_string(),
        ];

        // Default verbosity: the child's stdout is captured (never inherited)
        // and surfaced nowhere — no leak into the default log register.
        let (quiet_log, quiet_cap) = anodizer_core::log::StageLogger::with_capture(
            "notarize-test",
            anodizer_core::log::Verbosity::Normal,
        );
        let status = run_status_with_retry(&args, "echo-cmd", &quiet_log, &no_delay).unwrap();
        assert!(status.success());
        assert!(
            quiet_cap
                .all_messages()
                .into_iter()
                .all(|(_, m)| !m.contains("NOTARYCHATTER")),
            "default-verbosity run must NOT surface the child's stdout"
        );

        // Verbose: the captured stdout is streamed through the logger.
        let (verbose_log, verbose_cap) = anodizer_core::log::StageLogger::with_capture(
            "notarize-test",
            anodizer_core::log::Verbosity::Verbose,
        );
        let status = run_status_with_retry(&args, "echo-cmd", &verbose_log, &no_delay).unwrap();
        assert!(status.success());
        assert!(
            verbose_cap
                .all_messages()
                .into_iter()
                .any(|(_, m)| m.contains("NOTARYCHATTER")),
            "verbose run must surface the captured child stdout"
        );
    }

    /// `refresh_artifact_checksums` must cover signed DMG and PKG artifacts
    /// in addition to binaries — productsign and stapler rewrite bytes
    /// in place, so any cached `sha256` metadata is stale unless we
    /// recompute it after the signing pipeline.
    #[test]
    fn refresh_artifact_checksums_covers_dmg_and_pkg() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::tempdir().unwrap();

        let dmg_path = tmp.path().join("app.dmg");
        std::fs::write(&dmg_path, b"signed-dmg-bytes").unwrap();
        let pkg_path = tmp.path().join("app.pkg");
        std::fs::write(&pkg_path, b"signed-pkg-bytes").unwrap();

        let mut dmg_md = HashMap::new();
        dmg_md.insert("sha256".to_string(), "stale".to_string());
        let mut pkg_md = HashMap::new();
        pkg_md.insert("sha256".to_string(), "stale".to_string());

        let mut config = Config::default();
        config.project_name = "p".to_string();
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.artifacts.add(Artifact {
            name: "app.dmg".to_string(),
            path: PathBuf::from(&dmg_path),
            kind: ArtifactKind::DiskImage,
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "p".to_string(),
            metadata: dmg_md,
            size: None,
        });
        ctx.artifacts.add(Artifact {
            name: "app.pkg".to_string(),
            path: PathBuf::from(&pkg_path),
            kind: ArtifactKind::MacOsPackage,
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "p".to_string(),
            metadata: pkg_md,
            size: None,
        });

        let log = test_logger();
        refresh_artifact_checksums(&mut ctx, &log);

        for art in ctx.artifacts.all() {
            let sha = art.metadata.get("sha256").expect("sha256 set");
            assert_ne!(sha, "stale", "{} sha256 must be refreshed", art.name);
            assert_eq!(sha.len(), 64, "sha256 must be 64 hex chars");
        }
    }
}
