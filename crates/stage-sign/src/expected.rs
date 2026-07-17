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
/// set only — no extra configuration is required or consulted.
///
/// Returns a sorted, de-duplicated list of expected asset basenames. Empty
/// when signing is not configured, the sign stage was explicitly skipped, or
/// every sign config was intentionally waived for this run (see module docs
/// for the waiver order).
///
/// `release_ids` is the release block's `ids:` upload filter: a signature
/// inherits its SUBJECT's upload verdict (see `matches_id_filter`), so a
/// subject the release stage filters out contributes no expectation.
pub fn expected_signature_assets(
    ctx: &Context,
    crate_name: &str,
    release_ids: Option<&[String]>,
) -> Result<Vec<String>> {
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
            if !anodizer_core::artifact::matches_id_filter(artifact, release_ids) {
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
    let (sig_path, cert_path) = expected_output_paths(cfg, artifact_path, artifact_metadata, ctx)?;
    Ok((
        basename_of(&sig_path),
        cert_path.as_deref().map(basename_of),
    ))
}

/// Resolve the (signature, optional certificate) output PATHS one sign config
/// produces for one artifact — the dist-joined locations the sign stage
/// writes to. Shared naming source for [`expected_output_names`] (which takes
/// the basenames) and the standalone re-verification path (which needs the
/// on-disk files), so the two can never drift.
pub(crate) fn expected_output_paths(
    cfg: &SignConfig,
    artifact_path: &std::path::Path,
    artifact_metadata: &HashMap<String, String>,
    ctx: &Context,
) -> Result<(std::path::PathBuf, Option<std::path::PathBuf>)> {
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
    let sig_path = dist_joined(dist, &signature_str);
    let cert_path = certificate_str.as_deref().map(|c| dist_joined(dist, c));
    Ok((sig_path, cert_path))
}

/// Dist-join a rendered output path the way the sign stage registers it.
fn dist_joined(dist: &std::path::Path, rendered: &str) -> std::path::PathBuf {
    let resolved = std::path::PathBuf::from(rendered);
    if !resolved.starts_with(dist) {
        dist.join(&resolved)
    } else {
        resolved
    }
}

/// The asset basename of a resolved output path (the name the release
/// upload uses).
fn basename_of(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
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

    /// A combined `checksums.txt` artifact — the only Checksum kind that
    /// `artifacts: checksum` signs (split sidecars are never signed).
    fn combined_checksum(name: &str, crate_name: &str) -> Artifact {
        let mut a = artifact(ArtifactKind::Checksum, name, crate_name, None);
        a.metadata.insert(
            anodizer_core::artifact::COMBINED_CHECKSUM_META.to_string(),
            anodizer_core::artifact::COMBINED_CHECKSUM_VALUE.to_string(),
        );
        a
    }

    /// Portable no-op sign command: `true` on Unix, `cmd /c exit 0` on
    /// Windows (which has no `true` binary).
    pub(super) fn noop_cmd() -> (Option<String>, Option<Vec<String>>) {
        if cfg!(windows) {
            (
                Some("cmd".to_string()),
                Some(vec!["/c".to_string(), "exit".to_string(), "0".to_string()]),
            )
        } else {
            (Some("true".to_string()), Some(vec![]))
        }
    }

    fn checksum_sign(artifacts: &str) -> SignConfig {
        let (cmd, args) = noop_cmd();
        SignConfig {
            id: Some("default".to_string()),
            artifacts: Some(artifacts.to_string()),
            cmd,
            args,
            ..Default::default()
        }
    }

    #[test]
    fn signing_enabled_expects_per_artifact_signature() {
        let mut ctx = TestContextBuilder::new()
            .signs(vec![checksum_sign("checksum")])
            .build();
        ctx.artifacts
            .add(combined_checksum("app_checksums.txt", "app"));
        ctx.artifacts
            .add(artifact(ArtifactKind::Archive, "app.tar.gz", "app", None));

        let expected = expected_signature_assets(&ctx, "app", None).expect("derivation");
        // `artifacts: checksum` signs only the COMBINED checksums file.
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

        let expected = expected_signature_assets(&ctx, "app", None).expect("derivation");
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
        ctx.artifacts.add(combined_checksum("sums.txt", "app"));

        let expected = expected_signature_assets(&ctx, "app", None).expect("derivation");
        assert_eq!(expected, vec!["sums.txt.asc".to_string()]);
    }

    #[test]
    fn no_signs_config_creates_no_expectations() {
        let mut ctx = TestContextBuilder::new().build();
        ctx.artifacts
            .add(artifact(ArtifactKind::Checksum, "sums.txt", "app", None));
        let expected = expected_signature_assets(&ctx, "app", None).expect("derivation");
        assert!(expected.is_empty());
    }

    #[test]
    fn artifacts_none_creates_no_expectations() {
        let mut ctx = TestContextBuilder::new()
            .signs(vec![checksum_sign("none")])
            .build();
        ctx.artifacts
            .add(artifact(ArtifactKind::Checksum, "sums.txt", "app", None));
        let expected = expected_signature_assets(&ctx, "app", None).expect("derivation");
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
        let expected = expected_signature_assets(&ctx, "app", None).expect("derivation");
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
        let expected = expected_signature_assets(&ctx, "app", None).expect("derivation");
        assert!(expected.is_empty());
    }

    #[test]
    fn skip_record_for_other_label_does_not_waive() {
        let mut ctx = TestContextBuilder::new()
            .signs(vec![checksum_sign("checksum")])
            .build();
        ctx.artifacts.add(combined_checksum("sums.txt", "app"));
        ctx.remember_skip("sign", "some-other-config", "artifacts: none");
        let expected = expected_signature_assets(&ctx, "app", None).expect("derivation");
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
        let expected = expected_signature_assets(&ctx, "app", None).expect("derivation");
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
        let expected = expected_signature_assets(&ctx, "app", None).expect("derivation");
        assert_eq!(expected, vec!["keep.tar.gz.sig".to_string()]);
    }

    #[test]
    fn expectations_resolve_per_crate() {
        // Workspace modes: each published crate gets only its own artifacts'
        // signature expectations.
        let mut ctx = TestContextBuilder::new()
            .signs(vec![checksum_sign("checksum")])
            .build();
        ctx.artifacts
            .add(combined_checksum("a_checksums.txt", "crate-a"));
        ctx.artifacts
            .add(combined_checksum("b_checksums.txt", "crate-b"));

        let a = expected_signature_assets(&ctx, "crate-a", None).expect("derivation");
        let b = expected_signature_assets(&ctx, "crate-b", None).expect("derivation");
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
        let expected = expected_signature_assets(&ctx, "app", None).expect("derivation");
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
        ctx.artifacts
            .add(combined_checksum("app_checksums.txt", "app"));
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

        let predicted = expected_signature_assets(&ctx, "app", None).expect("derivation");

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

#[cfg(test)]
mod subject_provenance_tests {
    use super::*;
    use crate::process::{ArtifactFilter, process_sign_configs};
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::TestContextBuilder;

    fn id_archive(name: &str, id: &str) -> Artifact {
        let mut metadata = HashMap::new();
        metadata.insert("id".to_string(), id.to_string());
        Artifact {
            kind: ArtifactKind::Archive,
            name: name.to_string(),
            path: std::path::PathBuf::from(name),
            target: None,
            crate_name: "app".to_string(),
            metadata,
            size: None,
        }
    }

    fn archive_sign() -> SignConfig {
        let (cmd, args) = super::tests::noop_cmd();
        SignConfig {
            id: Some("default".to_string()),
            artifacts: Some("archive".to_string()),
            cmd,
            args,
            ..Default::default()
        }
    }

    #[test]
    fn sign_registrations_carry_subject_provenance() {
        // The registered Signature/Certificate artifacts must record the
        // signed artifact's kind and build id so the release `ids:` filter
        // gives them the same upload verdict as their subject.
        let cfg = SignConfig {
            certificate: Some("{{ .Artifact }}.pem".to_string()),
            ..archive_sign()
        };
        let mut ctx = TestContextBuilder::new().signs(vec![cfg]).build();
        ctx.artifacts.add(id_archive("keep.tar.gz", "keep"));

        let log = ctx.logger("sign");
        let cfgs = ctx.config.signs.clone();
        process_sign_configs(&cfgs, &mut ctx, &log, ArtifactFilter::FromConfig, "sign")
            .expect("sign run");

        for kind in [ArtifactKind::Signature, ArtifactKind::Certificate] {
            let arts = ctx.artifacts.by_kind(kind);
            assert_eq!(arts.len(), 1, "{kind:?} registered");
            assert_eq!(
                arts[0].metadata.get("subject_kind").map(String::as_str),
                Some("archive"),
                "{kind:?} records its subject's kind"
            );
            assert_eq!(
                arts[0].metadata.get("id").map(String::as_str),
                Some("keep"),
                "{kind:?} inherits its subject's build id"
            );
        }
    }

    #[test]
    fn release_ids_subject_verdict_filters_expectations() {
        let mut ctx = TestContextBuilder::new()
            .signs(vec![archive_sign()])
            .build();
        ctx.artifacts.add(id_archive("keep.tar.gz", "keep"));
        ctx.artifacts.add(id_archive("drop.tar.gz", "drop"));

        let ids = vec!["keep".to_string()];
        let expected = expected_signature_assets(&ctx, "app", Some(&ids)).expect("derivation");
        assert_eq!(
            expected,
            vec!["keep.tar.gz.sig".to_string()],
            "only the ids-included subject contributes a signature expectation"
        );
    }

    #[test]
    fn sign_of_sbom_inherits_transitive_verdict() {
        // Signing SBOMs (artifacts: sbom or all): the signature must carry
        // its subject SBOM's own verdict record, transitively — a sig of a
        // subject-less `any` SBOM carries no record (always uploads), and a
        // sig of a per-archive SBOM answers to the archive's build id.
        let cfg = SignConfig {
            artifacts: Some("sbom".to_string()),
            ..archive_sign()
        };
        let mut ctx = TestContextBuilder::new().signs(vec![cfg]).build();
        let sbom = |name: &str, meta: &[(&str, &str)]| Artifact {
            kind: ArtifactKind::Sbom,
            name: name.to_string(),
            path: std::path::PathBuf::from(name),
            target: None,
            crate_name: "app".to_string(),
            metadata: meta
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            size: None,
        };
        ctx.artifacts
            .add(sbom("project.cdx.json", &[("sbom_id", "default")]));
        ctx.artifacts.add(sbom(
            "keep.tar.gz.cdx.json",
            &[("subject_kind", "archive"), ("id", "keep")],
        ));
        ctx.artifacts.add(sbom(
            "drop.tar.gz.cdx.json",
            &[("subject_kind", "archive"), ("id", "drop")],
        ));

        // Derivation under the ids filter: the subject-less SBOM's sig and
        // the kept archive's SBOM sig are expected; the dropped one is not.
        let ids = vec!["keep".to_string()];
        let expected = expected_signature_assets(&ctx, "app", Some(&ids)).expect("derivation");
        assert_eq!(
            expected,
            vec![
                "keep.tar.gz.cdx.json.sig".to_string(),
                "project.cdx.json.sig".to_string()
            ]
        );

        let log = ctx.logger("sign");
        let cfgs = ctx.config.signs.clone();
        process_sign_configs(&cfgs, &mut ctx, &log, ArtifactFilter::FromConfig, "sign")
            .expect("sign run");

        let by_name = |n: &str| -> anodizer_core::artifact::Artifact {
            ctx.artifacts
                .by_kind(ArtifactKind::Signature)
                .into_iter()
                .find(|a| a.name == n)
                .unwrap_or_else(|| panic!("signature '{n}' registered"))
                .clone()
        };
        let any_sig = by_name("project.cdx.json.sig");
        assert!(
            !any_sig.metadata.contains_key("subject_kind"),
            "sig of a subject-less SBOM carries no record: {:?}",
            any_sig.metadata
        );
        let keep_sig = by_name("keep.tar.gz.cdx.json.sig");
        assert_eq!(
            keep_sig.metadata.get("subject_kind").map(String::as_str),
            Some("archive")
        );
        assert_eq!(
            keep_sig.metadata.get("id").map(String::as_str),
            Some("keep")
        );
        let drop_sig = by_name("drop.tar.gz.cdx.json.sig");

        // Upload verdicts under the ids filter must match the derivation.
        use anodizer_core::artifact::matches_id_filter;
        assert!(
            matches_id_filter(&any_sig, Some(&ids)),
            "any-sbom sig uploads"
        );
        assert!(
            matches_id_filter(&keep_sig, Some(&ids)),
            "kept-subject sig uploads"
        );
        assert!(
            !matches_id_filter(&drop_sig, Some(&ids)),
            "excluded-subject sig must not upload"
        );
    }

    #[test]
    fn zero_match_ids_filter_warns_loudly() {
        // A sign config whose ids filter eliminates every kind-matched
        // artifact silently signs nothing — the stage must warn.
        let cfg = SignConfig {
            ids: Some(vec!["no-such-id".to_string()]),
            ..archive_sign()
        };
        let mut ctx = TestContextBuilder::new().signs(vec![cfg]).build();
        ctx.artifacts.add(id_archive("app.tar.gz", "real-id"));

        let (log, capture) = anodizer_core::log::StageLogger::with_capture(
            "sign",
            anodizer_core::log::Verbosity::Quiet,
        );
        let cfgs = ctx.config.signs.clone();
        process_sign_configs(&cfgs, &mut ctx, &log, ArtifactFilter::FromConfig, "sign")
            .expect("sign run");

        assert!(
            capture
                .warn_messages()
                .iter()
                .any(|m| m.contains("matched no artifacts")),
            "zero-match ids filter must warn: {:?}",
            capture.all_messages()
        );
    }

    #[test]
    fn explicit_id_matching_fallback_label_rejected() {
        use anodizer_core::stage::Stage;
        let cfg = SignConfig {
            id: Some("sign[0]".to_string()),
            ..archive_sign()
        };
        let mut ctx = TestContextBuilder::new().signs(vec![cfg]).build();
        let err = crate::SignStage
            .run(&mut ctx)
            .expect_err("reserved positional id must be rejected");
        assert!(
            format!("{err:#}").contains("reserved positional label pattern"),
            "error explains the collision: {err:#}"
        );
    }

    #[test]
    fn binary_signs_explicit_fallback_id_rejected() {
        use anodizer_core::stage::Stage;
        let cfg = SignConfig {
            id: Some("binary-sign[2]".to_string()),
            ..archive_sign()
        };
        let mut ctx = TestContextBuilder::new().binary_signs(vec![cfg]).build();
        let err = crate::BinarySignStage
            .run(&mut ctx)
            .expect_err("reserved positional id must be rejected");
        assert!(format!("{err:#}").contains("reserved positional label pattern"));
    }

    #[test]
    fn normal_explicit_ids_accepted_and_cross_label_ids_allowed() {
        use anodizer_core::stage::Stage;
        // "binary-sign[0]" in the SIGNS list is not that list's fallback
        // shape ("sign[N]"), so it does not alias any signs skip record.
        let cfgs = vec![
            SignConfig {
                id: Some("gpg-checksums".to_string()),
                ..archive_sign()
            },
            SignConfig {
                id: Some("binary-sign[0]".to_string()),
                ..archive_sign()
            },
        ];
        let mut ctx = TestContextBuilder::new().signs(cfgs).build();
        assert!(crate::SignStage.run(&mut ctx).is_ok());
    }
}
