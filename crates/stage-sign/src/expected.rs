//! Config-derived signature-asset expectations.
//!
//! [`expected_signature_assets`] answers the post-publish gate's question:
//! given the resolved `signs:` config and the artifact set this run produced,
//! which signature / certificate ASSET NAMES must exist on the published
//! release? It deliberately does NOT consult the registry's
//! `Signature`/`Certificate` artifacts — those exist only when the sign stage
//! actually executed, and a release gate that derives its expectations from
//! the sign stage's own output cannot detect the sign stage silently
//! producing nothing (anodizer v0.8.0 shipped with zero signature assets and
//! the asset-existence check passed, because the skipped sign stage had
//! registered no signature artifacts to expect).
//!
//! ## When a sign config creates NO expectations
//!
//! 1. The whole sign stage was skipped via `--skip=sign` — explicit operator
//!    intent for this run.
//! 2. The run's own skip record ([`Context::skip_memento`]) marks the config
//!    as intentionally skipped (`if:` falsy, `artifacts: none`). The memento
//!    is the authoritative record of THIS run's decision: it was written at
//!    the moment the sign stage evaluated the condition, so it is immune to
//!    template-variable drift between sign time and verify time.
//! 3. Fallback, only when no memento entry exists (the sign stage did not run
//!    in this process — e.g. a future standalone verify entry point):
//!    re-evaluating the config's `if:` with the same evaluator the sign stage
//!    uses yields falsy.
//!
//! `binary_signs:` are intentionally absent here: their outputs carry the
//! `binary_sign` metadata marker and are excluded from release upload
//! (`is_binary_sign_output`), so they can never be missing release assets.
//! `docker_signs:` signatures live in the registry, not on the release.

use std::collections::HashMap;

use anyhow::Result;

use anodizer_core::config::SignConfig;
use anodizer_core::context::Context;

use crate::helpers::{
    expand_shell_vars, resolve_signature_path, should_sign_artifact, sign_ids_match,
};

/// Derive the signature / certificate asset names the `signs:` config demands
/// for `crate_name`'s published release, from config + the produced artifact
/// set only (rule: derive from context the tool already has; no new config).
///
/// Returns a sorted, de-duplicated list of expected asset basenames. Empty
/// when signing is not configured, the sign stage was explicitly skipped, or
/// every sign config was intentionally waived for this run (see module docs
/// for the waiver order).
pub fn expected_signature_assets(ctx: &Context, crate_name: &str) -> Result<Vec<String>> {
    if ctx.should_skip("sign") {
        return Ok(Vec::new());
    }
    let skips = ctx.skip_memento.snapshot();
    let mut expected: Vec<String> = Vec::new();

    for (sign_idx, cfg) in ctx.config.signs.iter().enumerate() {
        // Must mirror process_sign_configs' sub_label derivation exactly, or
        // a recorded skip would fail to match and resurrect expectations the
        // run already waived.
        let sub_label = cfg
            .id
            .clone()
            .unwrap_or_else(|| format!("sign[{sign_idx}]"));
        if skips
            .iter()
            .any(|e| e.stage == "sign" && e.label == sub_label)
        {
            continue;
        }
        let proceed = anodizer_core::config::evaluate_if_condition(
            cfg.if_condition.as_deref(),
            &format!("sign '{sub_label}' (expected-asset derivation)"),
            |t| ctx.render_template(t),
        )?;
        if !proceed {
            continue;
        }
        let filter = cfg.resolved_artifacts(SignConfig::DEFAULT_ARTIFACTS);
        if filter == "none" {
            continue;
        }

        for artifact in ctx.artifacts.all() {
            if artifact.crate_name != crate_name {
                continue;
            }
            if !should_sign_artifact(artifact.kind, filter)? {
                continue;
            }
            if !sign_ids_match(&artifact.metadata, cfg.ids.as_ref()) {
                continue;
            }
            let (sig_name, cert_name) =
                expected_output_names(cfg, &artifact.path, &artifact.metadata, ctx)?;
            expected.push(sig_name);
            if let Some(cert) = cert_name {
                expected.push(cert);
            }
        }
    }

    expected.sort();
    expected.dedup();
    Ok(expected)
}

/// Resolve the (signature, optional certificate) asset basenames one sign
/// config produces for one artifact.
///
/// Mirrors the naming steps of `process_sign_configs` (render the
/// `signature:` / `certificate:` templates with `{{ .Artifact }}`
/// pre-substituted, expand `$var` references, join under `dist/` unless
/// already there, take the basename). Kept as a parallel implementation
/// rather than extracted from `process.rs` because the execution path also
/// needs the PRE-expansion strings for `$signature` argv substitution; the
/// `expected_signature_assets_match_sign_stage_registrations` equivalence
/// test in `tests.rs` pins the two paths together so they cannot drift.
fn expected_output_names(
    cfg: &SignConfig,
    artifact_path: &std::path::Path,
    artifact_metadata: &HashMap<String, String>,
    ctx: &Context,
) -> Result<(String, Option<String>)> {
    use anyhow::Context as _;

    let artifact_str = artifact_path.to_string_lossy();
    let artifact_name = artifact_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let artifact_id = artifact_metadata
        .get("id")
        .map(|s| s.as_str())
        .unwrap_or("");

    let signature_str = resolve_signature_path(
        cfg,
        &artifact_str,
        ctx,
        SignConfig::DEFAULT_SIGNATURE_TEMPLATE,
    )?;

    let certificate_str = cfg
        .certificate
        .as_ref()
        .map(|tmpl| {
            let preprocessed = tmpl
                .replace("{{ .Artifact }}", &artifact_str)
                .replace("{{ Artifact }}", &artifact_str);
            ctx.render_template(&preprocessed).with_context(|| {
                format!(
                    "sign: render certificate template '{}' for artifact {}",
                    tmpl, artifact_str
                )
            })
        })
        .transpose()?;

    let certificate_for_vars = certificate_str.clone();
    let shell_vars: HashMap<&str, &str> = HashMap::from([
        ("artifact", artifact_str.as_ref()),
        ("signature", signature_str.as_str()),
        ("certificate", certificate_for_vars.as_deref().unwrap_or("")),
        (
            "digest",
            artifact_metadata
                .get("digest")
                .map(|s| s.as_str())
                .unwrap_or(""),
        ),
        ("artifactName", artifact_name),
        ("artifactID", artifact_id),
    ]);

    let signature_str = expand_shell_vars(&signature_str, &shell_vars);
    let certificate_str = certificate_str.map(|c| expand_shell_vars(&c, &shell_vars));

    let dist = &ctx.config.dist;
    let sig_name = basename_under_dist(dist, &signature_str);
    let cert_name = certificate_str
        .as_deref()
        .map(|c| basename_under_dist(dist, c));
    Ok((sig_name, cert_name))
}

/// Dist-join a rendered output path the way the sign stage registers it, then
/// take the asset basename (the name the release upload uses).
fn basename_under_dist(dist: &std::path::Path, rendered: &str) -> String {
    let resolved = std::path::PathBuf::from(rendered);
    let joined = if !resolved.starts_with(dist) {
        dist.join(&resolved)
    } else {
        resolved
    };
    joined
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| joined.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{ArtifactFilter, process_sign_configs};
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::TestContextBuilder;

    fn artifact(kind: ArtifactKind, name: &str, crate_name: &str, id: Option<&str>) -> Artifact {
        let mut metadata = HashMap::new();
        if let Some(id) = id {
            metadata.insert("id".to_string(), id.to_string());
        }
        Artifact {
            kind,
            name: name.to_string(),
            path: std::path::PathBuf::from(name),
            target: None,
            crate_name: crate_name.to_string(),
            metadata,
            size: None,
        }
    }

    fn checksum_sign(artifacts: &str) -> SignConfig {
        SignConfig {
            id: Some("default".to_string()),
            artifacts: Some(artifacts.to_string()),
            cmd: Some("true".to_string()),
            args: Some(vec![]),
            ..Default::default()
        }
    }

    #[test]
    fn signing_enabled_expects_per_artifact_signature() {
        let mut ctx = TestContextBuilder::new()
            .signs(vec![checksum_sign("checksum")])
            .build();
        ctx.artifacts.add(artifact(
            ArtifactKind::Checksum,
            "app_checksums.txt",
            "app",
            None,
        ));
        ctx.artifacts
            .add(artifact(ArtifactKind::Archive, "app.tar.gz", "app", None));

        let expected = expected_signature_assets(&ctx, "app").expect("derivation");
        // `artifacts: checksum` signs only the Checksum artifact.
        assert_eq!(expected, vec!["app_checksums.txt.sig".to_string()]);
    }

    #[test]
    fn certificate_template_adds_expected_certificate_asset() {
        let cfg = SignConfig {
            certificate: Some("{{ .Artifact }}.pem".to_string()),
            ..checksum_sign("archive")
        };
        let mut ctx = TestContextBuilder::new().signs(vec![cfg]).build();
        ctx.artifacts
            .add(artifact(ArtifactKind::Archive, "app.tar.gz", "app", None));

        let expected = expected_signature_assets(&ctx, "app").expect("derivation");
        assert_eq!(
            expected,
            vec!["app.tar.gz.pem".to_string(), "app.tar.gz.sig".to_string()]
        );
    }

    #[test]
    fn custom_signature_template_is_honored() {
        let cfg = SignConfig {
            signature: Some("{{ .Artifact }}.asc".to_string()),
            ..checksum_sign("checksum")
        };
        let mut ctx = TestContextBuilder::new().signs(vec![cfg]).build();
        ctx.artifacts
            .add(artifact(ArtifactKind::Checksum, "sums.txt", "app", None));

        let expected = expected_signature_assets(&ctx, "app").expect("derivation");
        assert_eq!(expected, vec!["sums.txt.asc".to_string()]);
    }

    #[test]
    fn no_signs_config_creates_no_expectations() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.artifacts
            .add(artifact(ArtifactKind::Checksum, "sums.txt", "app", None));
        let expected = expected_signature_assets(&ctx, "app").expect("derivation");
        assert!(expected.is_empty());
    }

    #[test]
    fn artifacts_none_creates_no_expectations() {
        let mut ctx = TestContextBuilder::new()
            .signs(vec![checksum_sign("none")])
            .build();
        ctx.artifacts
            .add(artifact(ArtifactKind::Checksum, "sums.txt", "app", None));
        let expected = expected_signature_assets(&ctx, "app").expect("derivation");
        assert!(expected.is_empty());
    }

    #[test]
    fn falsy_if_condition_creates_no_expectations() {
        let cfg = SignConfig {
            if_condition: Some("false".to_string()),
            ..checksum_sign("checksum")
        };
        let mut ctx = TestContextBuilder::new().signs(vec![cfg]).build();
        ctx.artifacts
            .add(artifact(ArtifactKind::Checksum, "sums.txt", "app", None));
        let expected = expected_signature_assets(&ctx, "app").expect("derivation");
        assert!(
            expected.is_empty(),
            "a sign config whose if: evaluated falsy must create no expectations"
        );
    }

    #[test]
    fn recorded_skip_memento_waives_expectations() {
        // The run's own skip record is the authoritative waiver: when the
        // sign stage recorded this config as intentionally skipped, the
        // derivation must not resurrect expectations for it.
        let mut ctx = TestContextBuilder::new()
            .signs(vec![checksum_sign("checksum")])
            .build();
        ctx.artifacts
            .add(artifact(ArtifactKind::Checksum, "sums.txt", "app", None));
        ctx.remember_skip("sign", "default", "`if` condition evaluated falsy");
        let expected = expected_signature_assets(&ctx, "app").expect("derivation");
        assert!(expected.is_empty());
    }

    #[test]
    fn skip_record_for_other_label_does_not_waive() {
        let mut ctx = TestContextBuilder::new()
            .signs(vec![checksum_sign("checksum")])
            .build();
        ctx.artifacts
            .add(artifact(ArtifactKind::Checksum, "sums.txt", "app", None));
        ctx.remember_skip("sign", "some-other-config", "artifacts: none");
        let expected = expected_signature_assets(&ctx, "app").expect("derivation");
        assert_eq!(expected, vec!["sums.txt.sig".to_string()]);
    }

    #[test]
    fn skip_sign_stage_flag_waives_all_expectations() {
        let mut ctx = TestContextBuilder::new()
            .signs(vec![checksum_sign("checksum")])
            .skip_stages(vec!["sign".to_string()])
            .build();
        ctx.artifacts
            .add(artifact(ArtifactKind::Checksum, "sums.txt", "app", None));
        let expected = expected_signature_assets(&ctx, "app").expect("derivation");
        assert!(
            expected.is_empty(),
            "--skip=sign is explicit operator intent; no expectations"
        );
    }

    #[test]
    fn ids_filter_limits_expectations() {
        let cfg = SignConfig {
            ids: Some(vec!["keep".to_string()]),
            ..checksum_sign("archive")
        };
        let mut ctx = TestContextBuilder::new().signs(vec![cfg]).build();
        ctx.artifacts.add(artifact(
            ArtifactKind::Archive,
            "keep.tar.gz",
            "app",
            Some("keep"),
        ));
        ctx.artifacts.add(artifact(
            ArtifactKind::Archive,
            "drop.tar.gz",
            "app",
            Some("drop"),
        ));
        let expected = expected_signature_assets(&ctx, "app").expect("derivation");
        assert_eq!(expected, vec!["keep.tar.gz.sig".to_string()]);
    }

    #[test]
    fn expectations_resolve_per_crate() {
        // Workspace modes: each published crate gets only its own artifacts'
        // signature expectations.
        let mut ctx = TestContextBuilder::new()
            .signs(vec![checksum_sign("checksum")])
            .build();
        ctx.artifacts.add(artifact(
            ArtifactKind::Checksum,
            "a_checksums.txt",
            "crate-a",
            None,
        ));
        ctx.artifacts.add(artifact(
            ArtifactKind::Checksum,
            "b_checksums.txt",
            "crate-b",
            None,
        ));

        let a = expected_signature_assets(&ctx, "crate-a").expect("derivation");
        let b = expected_signature_assets(&ctx, "crate-b").expect("derivation");
        assert_eq!(a, vec!["a_checksums.txt.sig".to_string()]);
        assert_eq!(b, vec!["b_checksums.txt.sig".to_string()]);
    }

    #[test]
    fn binary_signs_create_no_release_expectations() {
        // binary_signs outputs are excluded from release upload
        // (is_binary_sign_output), so the derivation must ignore them.
        let cfg = SignConfig {
            artifacts: Some("binary".to_string()),
            ..checksum_sign("binary")
        };
        let mut ctx = TestContextBuilder::new().binary_signs(vec![cfg]).build();
        ctx.artifacts.add(artifact(
            ArtifactKind::Binary,
            "app_linux_amd64",
            "app",
            None,
        ));
        let expected = expected_signature_assets(&ctx, "app").expect("derivation");
        assert!(expected.is_empty());
    }

    #[test]
    fn expected_signature_assets_match_sign_stage_registrations() {
        // Equivalence pin: the derivation is a parallel implementation of the
        // execution path's selection + naming (see expected_output_names).
        // Run the REAL sign driver (cmd `true`) over a mixed artifact set and
        // assert the derivation predicted exactly the Signature/Certificate
        // names the stage registered. Any drift between the two paths fails
        // here.
        let cfgs = vec![
            checksum_sign("checksum"),
            SignConfig {
                id: Some("cosign".to_string()),
                signature: Some("{{ .Artifact }}.bundle.sig".to_string()),
                certificate: Some("{{ .Artifact }}.pem".to_string()),
                ..checksum_sign("archive")
            },
        ];
        let mut ctx = TestContextBuilder::new().signs(cfgs).build();
        ctx.artifacts.add(artifact(
            ArtifactKind::Checksum,
            "app_checksums.txt",
            "app",
            None,
        ));
        ctx.artifacts.add(artifact(
            ArtifactKind::Archive,
            "app_linux_amd64.tar.gz",
            "app",
            None,
        ));
        ctx.artifacts.add(artifact(
            ArtifactKind::Archive,
            "app_darwin_arm64.tar.gz",
            "app",
            None,
        ));

        let predicted = expected_signature_assets(&ctx, "app").expect("derivation");

        let log = ctx.logger("sign");
        let sign_cfgs = ctx.config.signs.clone();
        process_sign_configs(
            &sign_cfgs,
            &mut ctx,
            &log,
            ArtifactFilter::FromConfig,
            "sign",
        )
        .expect("sign run");

        let mut registered: Vec<String> = ctx
            .artifacts
            .by_kind(ArtifactKind::Signature)
            .into_iter()
            .chain(ctx.artifacts.by_kind(ArtifactKind::Certificate))
            .map(|a| a.name.clone())
            .collect();
        registered.sort();
        registered.dedup();

        assert_eq!(
            predicted, registered,
            "config-derived expectations must equal what the sign stage registers"
        );
    }
}
