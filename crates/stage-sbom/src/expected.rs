//! Config-derived SBOM-asset expectations.
//!
//! [`expected_sbom_assets`] answers the post-publish gate's question: given
//! the resolved `sboms:` config and the artifact set this run produced, which
//! SBOM ASSET NAMES must exist on the published release? Like the sign-side
//! derivation it does NOT consult the registry's `Sbom` artifacts — those
//! exist only when the SBOM stage actually executed, and the gate exists to
//! catch a configured stage silently producing nothing.
//!
//! A config creates NO expectations when the stage was skipped via
//! `--skip=sbom`, when its `skip:` expression evaluates truthy, or when its
//! rendered document name is not predictable from config alone (glob
//! patterns, which expand against on-disk matches; absolute paths, which the
//! stage rejects).

use std::path::Path;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, matches_id_filter};
use anodizer_core::context::Context;

use crate::{builtin_format_and_extension, typed_artifact_kind};

/// Derive the SBOM asset names the `sboms:` config demands for `crate_name`'s
/// published release, from config + the produced artifact set only.
///
/// Returns a sorted, de-duplicated list of expected asset basenames. A
/// document about one artifact is registered under that artifact's crate
/// (see `run_sbom`), so its expectation attaches to the crate whose release
/// carries the subject. The whole-project documents — `artifacts: any`, and
/// the built-in generator's single workspace document — are registered under
/// the release owner crate (see `run_sbom_builtin`), so only that crate's
/// release expects them.
///
/// `release_ids` is the release block's `ids:` upload filter: an SBOM
/// inherits its SUBJECT artifact's upload verdict (see `matches_id_filter`),
/// so a subject the release stage filters out contributes no expectation.
/// Project-wide `artifacts: any` SBOMs have no subject and always upload.
pub fn expected_sbom_assets(
    ctx: &Context,
    crate_name: &str,
    release_ids: Option<&[String]>,
) -> Result<Vec<String>> {
    if ctx.should_skip("sbom") {
        return Ok(Vec::new());
    }
    let owns_repository_documents = crate_name == ctx.config.release_owner_crate_name();
    let project_name = &ctx.config.project_name;
    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());

    let mut expected: Vec<String> = Vec::new();

    for cfg in &ctx.config.sboms {
        let id = cfg.resolved_id();
        if let Some(ref d) = cfg.skip
            && d.try_evaluates_to_true(|s| ctx.render_template(s))
                .with_context(|| format!("sbom[{id}]: evaluate skip expression"))?
        {
            continue;
        }

        let artifacts_type = cfg.resolved_artifacts();
        let documents = cfg.resolved_documents(artifacts_type);
        let use_builtin = cfg.cmd.is_none() && cfg.args.is_none();

        if artifacts_type == "any" {
            if !owns_repository_documents {
                continue;
            }
            if use_builtin {
                let (_, extension) = builtin_format_and_extension(&documents);
                expected.push(format!("{project_name}-{version}.{extension}"));
            } else {
                // The external `any` path renders documents once against a
                // synthetic empty-path artifact tuple — bind the same
                // synthetic vars the stage binds (ArtifactName "artifact",
                // empty ArtifactID, no target).
                let vars = crate::artifact_template_vars(
                    ctx,
                    Path::new(""),
                    &std::collections::HashMap::new(),
                    None,
                    None,
                );
                for doc_tpl in &documents {
                    let rendered =
                        anodizer_core::template::render(doc_tpl, &vars).with_context(|| {
                            format!("sbom[{id}]: failed to render document template '{doc_tpl}'")
                        })?;
                    push_predictable_basename(&mut expected, &rendered);
                }
            }
            continue;
        }

        // The stage rejects typed-mode configs with more than one documents
        // entry in both generation modes (per-artifact rendering would
        // clobber on collision), so such a config can never have published a
        // release — it creates no expectations.
        if documents.len() > 1 {
            continue;
        }

        let matching: Vec<&Artifact> = if artifacts_type == "binary" {
            ctx.artifacts
                .binary_like_dedup()
                .into_iter()
                .filter(|a| matches_id_filter(a, cfg.ids.as_deref()))
                .filter(|a| matches_id_filter(a, release_ids))
                .filter(|a| use_builtin || a.crate_name == crate_name)
                .collect()
        } else {
            let kind = typed_artifact_kind(artifacts_type, id)?;
            // The stage skips a directory bundle (a macOS `.app`, registered
            // as an Installer that is never uploaded raw), so the gate must
            // not expect a document for it.
            ctx.artifacts
                .all()
                .iter()
                .filter(|a| a.kind == kind)
                .filter(|a| !anodizer_core::artifact::is_directory_bundle_artifact(a))
                .filter(|a| matches_id_filter(a, cfg.ids.as_deref()))
                .filter(|a| matches_id_filter(a, release_ids))
                .filter(|a| use_builtin || a.crate_name == crate_name)
                .collect()
        };

        // The built-in (Cargo.lock) generator's output is archive-independent,
        // so the stage emits a SINGLE workspace SBOM regardless of how many
        // archives match — named `<project>-<version>.<ext>` (the `any`
        // filename). The match scan above still gates it: zero matches means
        // the stage produced nothing, so demand nothing.
        if use_builtin {
            if owns_repository_documents && !matching.is_empty() {
                let (_, extension) = builtin_format_and_extension(&documents);
                expected.push(format!("{project_name}-{version}.{extension}"));
            }
            continue;
        }

        for artifact in matching {
            let vars = crate::artifact_template_vars(
                ctx,
                &artifact.path,
                &artifact.metadata,
                artifact.target.as_deref(),
                Some(artifact.kind),
            );
            // The external command path renders and registers EVERY documents
            // entry per artifact (genuinely-distinct per-archive scans).
            for doc_tpl in &documents {
                let rendered =
                    anodizer_core::template::render(doc_tpl, &vars).with_context(|| {
                        format!("sbom[{id}]: failed to render document template '{doc_tpl}'")
                    })?;
                push_predictable_basename(&mut expected, &rendered);
            }
        }
    }

    expected.sort();
    expected.dedup();
    Ok(expected)
}

/// Append the asset basename of a rendered document path when it is
/// predictable from config alone. Glob patterns expand against whatever the
/// generator wrote to disk and absolute paths are rejected by the stage, so
/// neither yields a name the gate can demand.
fn push_predictable_basename(out: &mut Vec<String>, rendered: &str) {
    if rendered.contains(['*', '?', '[']) {
        return;
    }
    let path = Path::new(rendered);
    if path.is_absolute() {
        return;
    }
    if let Some(name) = path.file_name() {
        out.push(name.to_string_lossy().into_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::artifact::ArtifactKind;
    use anodizer_core::config::SbomConfig;
    use anodizer_core::stage::Stage;
    use anodizer_core::test_helpers::TestContextBuilder;
    use std::collections::HashMap;

    fn archive(name: &str, crate_name: &str, target: Option<&str>) -> Artifact {
        Artifact {
            kind: ArtifactKind::Archive,
            name: name.to_string(),
            path: std::path::PathBuf::from(name),
            target: target.map(str::to_string),
            crate_name: crate_name.to_string(),
            metadata: HashMap::new(),
            size: None,
        }
    }

    fn per_archive_cfg() -> SbomConfig {
        SbomConfig {
            id: Some("default".to_string()),
            documents: Some(vec!["{{ .ArtifactName }}.cdx.json".to_string()]),
            artifacts: Some("archive".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn builtin_archive_expects_single_workspace_document() {
        // The built-in generator emits ONE archive-independent workspace SBOM,
        // not one per archive — so the derivation predicts a single
        // `<project>-<version>.<ext>` document regardless of archive count.
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .tag("v1.0.0")
            .add_sbom(per_archive_cfg())
            .build();
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts
            .add(archive("app-1.0-linux-amd64.tar.gz", "app", None));
        ctx.artifacts
            .add(archive("app-1.0-darwin-arm64.tar.gz", "app", None));

        let expected = expected_sbom_assets(&ctx, "app", None).expect("derivation");
        assert_eq!(expected, vec!["app-1.0.0.cdx.json".to_string()]);
    }

    #[test]
    fn skip_true_creates_no_expectations() {
        let cfg = SbomConfig {
            skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
            ..per_archive_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .add_sbom(cfg)
            .build();
        ctx.artifacts.add(archive("app.tar.gz", "app", None));
        let expected = expected_sbom_assets(&ctx, "app", None).expect("derivation");
        assert!(expected.is_empty());
    }

    #[test]
    fn skip_sbom_stage_flag_waives_expectations() {
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .add_sbom(per_archive_cfg())
            .skip_stages(vec!["sbom".to_string()])
            .build();
        ctx.artifacts.add(archive("app.tar.gz", "app", None));
        let expected = expected_sbom_assets(&ctx, "app", None).expect("derivation");
        assert!(expected.is_empty());
    }

    #[test]
    fn non_owner_crate_gets_no_workspace_sbom_expectations() {
        // The built-in generator's single workspace document is registered
        // under the release owner crate; another crate's release never
        // carries it, whatever archives that crate produced.
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .add_sbom(per_archive_cfg())
            .build();
        ctx.artifacts
            .add(archive("other.tar.gz", "other-crate", None));
        let expected = expected_sbom_assets(&ctx, "other-crate", None).expect("derivation");
        assert!(expected.is_empty());
    }

    #[test]
    fn workspace_sbom_expectations_follow_the_release_owner_not_the_project_name() {
        // Lockstep workspace where `project_name` names the SECOND crate with
        // a `release:` block: the stage registers the workspace document under
        // the first one, so that crate's release expects it and the project
        // crate's release does not.
        use anodizer_core::config::{CrateConfig, ReleaseConfig};
        let release = || {
            Some(ReleaseConfig {
                github: Some(anodizer_core::config::ScmRepoConfig {
                    owner: "o".to_string(),
                    name: "r".to_string(),
                    token: None,
                }),
                ..Default::default()
            })
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("ws-core")
            .tag("v1.0.0")
            .add_sbom(per_archive_cfg())
            .crates(vec![
                CrateConfig {
                    name: "ws-cli".to_string(),
                    path: "crates/cli".to_string(),
                    release: release(),
                    ..Default::default()
                },
                CrateConfig {
                    name: "ws-core".to_string(),
                    path: "crates/core".to_string(),
                    release: release(),
                    ..Default::default()
                },
            ])
            .build();
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts
            .add(archive("ws-cli-1.0.0.tar.gz", "ws-cli", None));
        ctx.artifacts
            .add(archive("ws-core-1.0.0.tar.gz", "ws-core", None));
        assert_eq!(ctx.config.release_owner_crate_name(), "ws-cli");

        let owner = expected_sbom_assets(&ctx, "ws-cli", None).expect("derivation");
        assert_eq!(owner, vec!["ws-core-1.0.0.cdx.json".to_string()]);
        let project = expected_sbom_assets(&ctx, "ws-core", None).expect("derivation");
        assert!(project.is_empty(), "{project:?}");
    }

    #[test]
    fn per_subject_external_sbom_expectations_follow_the_subject_crate() {
        // An external generator writes one document per archive, registered
        // under the archive's crate: each crate's release expects the
        // documents of its own archives and none of its siblings'.
        let cfg = SbomConfig {
            cmd: Some("syft".to_string()),
            ..per_archive_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("ws-cli")
            .add_sbom(cfg)
            .build();
        ctx.artifacts
            .add(archive("ws-cli-1.0.0.tar.gz", "ws-cli", None));
        ctx.artifacts
            .add(archive("ws-core-1.0.0.tar.gz", "ws-core", None));
        assert_eq!(
            expected_sbom_assets(&ctx, "ws-cli", None).expect("derivation"),
            vec!["ws-cli-1.0.0.tar.gz.cdx.json".to_string()]
        );
        assert_eq!(
            expected_sbom_assets(&ctx, "ws-core", None).expect("derivation"),
            vec!["ws-core-1.0.0.tar.gz.cdx.json".to_string()]
        );
    }

    #[test]
    fn glob_documents_create_no_expectations() {
        let cfg = SbomConfig {
            cmd: Some("syft".to_string()),
            documents: Some(vec!["*.spdx.json".to_string()]),
            ..per_archive_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .add_sbom(cfg)
            .build();
        ctx.artifacts.add(archive("app.tar.gz", "app", None));
        let expected = expected_sbom_assets(&ctx, "app", None).expect("derivation");
        assert!(
            expected.is_empty(),
            "glob document names are not predictable from config; no expectations"
        );
    }

    #[test]
    fn builtin_any_expects_project_version_document() {
        let cfg = SbomConfig {
            artifacts: Some("any".to_string()),
            documents: None,
            ..per_archive_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .tag("v1.2.3")
            .add_sbom(cfg)
            .build();
        ctx.template_vars_mut().set("Version", "1.2.3");
        let expected = expected_sbom_assets(&ctx, "app", None).expect("derivation");
        assert_eq!(expected, vec!["app-1.2.3.cdx.json".to_string()]);
    }

    #[test]
    fn expected_sbom_assets_match_external_typed_stage_registrations() {
        // Equivalence pin for the external-command TYPED path: the stage
        // renders the documents template per matched artifact and registers
        // the on-disk outputs; the derivation must predict the same names.
        let dist = tempfile::tempdir().expect("tempdir");
        let cfg = SbomConfig {
            id: Some("ext".to_string()),
            cmd: Some("sh".to_string()),
            args: Some(vec!["-c".to_string(), "echo x > \"$document\"".to_string()]),
            documents: Some(vec!["{{ .ArtifactName }}.spdx.json".to_string()]),
            artifacts: Some("archive".to_string()),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .tag("v1.0.0")
            .dist(dist.path().to_path_buf())
            .add_sbom(cfg)
            .build();
        ctx.artifacts.add(archive("app-one.tar.gz", "app", None));
        ctx.artifacts.add(archive("app-two.tar.gz", "app", None));

        let predicted = expected_sbom_assets(&ctx, "app", None).expect("derivation");

        crate::SbomStage.run(&mut ctx).expect("sbom stage run");

        let mut registered: Vec<String> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Sbom)
            .map(|a| a.name.clone())
            .collect();
        registered.sort();
        registered.dedup();
        assert_eq!(
            predicted, registered,
            "external typed derivation must equal what the stage registers"
        );
        assert_eq!(predicted.len(), 2, "one SBOM per matched archive");
    }

    #[test]
    fn expected_sbom_assets_match_external_any_stage_registrations() {
        // Equivalence pin for the external-command `artifacts: any` path:
        // EVERY documents entry is rendered once against the synthetic
        // empty-path artifact (ArtifactName "artifact"), and the derivation
        // must bind the same synthetic vars the stage binds.
        let dist = tempfile::tempdir().expect("tempdir");
        let cfg = SbomConfig {
            id: Some("ext-any".to_string()),
            cmd: Some("sh".to_string()),
            args: Some(vec![
                "-c".to_string(),
                "echo x > \"$document0\"; echo x > \"$document1\"".to_string(),
            ]),
            documents: Some(vec![
                "{{ .ArtifactName }}-one.spdx.json".to_string(),
                "{{ .ArtifactName }}-two.spdx.json".to_string(),
            ]),
            artifacts: Some("any".to_string()),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .tag("v1.0.0")
            .dist(dist.path().to_path_buf())
            .add_sbom(cfg)
            .build();

        let predicted = expected_sbom_assets(&ctx, "app", None).expect("derivation");
        assert_eq!(
            predicted,
            vec![
                "artifact-one.spdx.json".to_string(),
                "artifact-two.spdx.json".to_string()
            ],
            "synthetic ArtifactName binding must match the stage's"
        );

        crate::SbomStage.run(&mut ctx).expect("sbom stage run");

        let mut registered: Vec<String> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Sbom)
            .map(|a| a.name.clone())
            .collect();
        registered.sort();
        registered.dedup();
        assert_eq!(
            predicted, registered,
            "external `any` derivation must equal what the stage registers"
        );
    }

    #[test]
    fn typed_external_multi_document_config_rejected_on_both_sides() {
        // Typed-mode external configs with >1 documents are rejected by the
        // stage (per-artifact rendering would clobber on collision), so the
        // derivation mirrors the constraint by creating no expectations —
        // such a config can never have published a release.
        let dist = tempfile::tempdir().expect("tempdir");
        let cfg = SbomConfig {
            id: Some("ext".to_string()),
            cmd: Some("sh".to_string()),
            args: Some(vec!["-c".to_string(), "echo x > \"$document\"".to_string()]),
            documents: Some(vec![
                "{{ .ArtifactName }}.spdx.json".to_string(),
                "{{ .ArtifactName }}.cdx.json".to_string(),
            ]),
            artifacts: Some("archive".to_string()),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .tag("v1.0.0")
            .dist(dist.path().to_path_buf())
            .add_sbom(cfg)
            .build();
        ctx.artifacts.add(archive("app.tar.gz", "app", None));

        let predicted = expected_sbom_assets(&ctx, "app", None).expect("derivation");
        assert!(
            predicted.is_empty(),
            "rejected config shape must create no expectations: {predicted:?}"
        );

        let err = crate::SbomStage
            .run(&mut ctx)
            .expect_err("typed external multi-document config must be rejected");
        assert!(
            format!("{err:#}").contains("multiple SBOM outputs"),
            "stage names the constraint: {err:#}"
        );
    }

    #[test]
    fn typed_builtin_multi_document_config_rejected_on_both_sides() {
        // Built-in mode used to silently truncate a typed config's extra
        // documents entries to documents[0]; the rejection now covers both
        // generation modes, and the derivation mirrors it.
        let dist = tempfile::tempdir().expect("tempdir");
        let cfg = SbomConfig {
            documents: Some(vec![
                "{{ .ArtifactName }}.cdx.json".to_string(),
                "{{ .ArtifactName }}.spdx.json".to_string(),
            ]),
            ..per_archive_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .tag("v1.0.0")
            .dist(dist.path().to_path_buf())
            .add_sbom(cfg)
            .build();
        ctx.artifacts.add(archive("app.tar.gz", "app", None));

        let predicted = expected_sbom_assets(&ctx, "app", None).expect("derivation");
        assert!(
            predicted.is_empty(),
            "rejected config shape must create no expectations: {predicted:?}"
        );

        let err = crate::SbomStage
            .run(&mut ctx)
            .expect_err("typed builtin multi-document config must be rejected");
        assert!(
            format!("{err:#}").contains("multiple SBOM outputs"),
            "stage names the constraint: {err:#}"
        );
    }

    #[test]
    fn mixed_target_artifacts_render_hermetically_on_both_sides() {
        // A targeted artifact processed FIRST must not leak its Os/Arch into
        // the rendering of a later no-target artifact: the stage binds
        // per-artifact vars on a clone, never on the shared context. The
        // conditional template makes a leak visible: under leaky binding the
        // no-target archive would render "...-linux.cdx.json".
        //
        // Per-artifact document rendering is exclusive to the external-command
        // path (the built-in generator emits one archive-independent workspace
        // SBOM), so this hermeticity contract is pinned against `cmd:`.
        let dist = tempfile::tempdir().expect("tempdir");
        let cfg = SbomConfig {
            id: Some("default".to_string()),
            cmd: Some("sh".to_string()),
            args: Some(vec!["-c".to_string(), "echo x > \"$document\"".to_string()]),
            documents: Some(vec![
                "{{ .ArtifactName }}{% if Os %}-{{ Os }}{% endif %}.cdx.json".to_string(),
            ]),
            artifacts: Some("archive".to_string()),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .tag("v1.0.0")
            .dist(dist.path().to_path_buf())
            .add_sbom(cfg)
            .build();
        ctx.artifacts.add(archive(
            "with-target.tar.gz",
            "app",
            Some("x86_64-unknown-linux-gnu"),
        ));
        ctx.artifacts.add(archive("no-target.tar.gz", "app", None));

        let predicted = expected_sbom_assets(&ctx, "app", None).expect("derivation");

        crate::SbomStage.run(&mut ctx).expect("sbom stage run");

        let mut registered: Vec<String> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Sbom)
            .map(|a| a.name.clone())
            .collect();
        registered.sort();
        registered.dedup();

        assert_eq!(predicted, registered, "derivation and stage must agree");
        assert!(
            registered.contains(&"with-target.tar.gz-linux.cdx.json".to_string()),
            "targeted artifact renders its own Os: {registered:?}"
        );
        assert!(
            registered.contains(&"no-target.tar.gz.cdx.json".to_string()),
            "no-target artifact must NOT inherit the previous artifact's Os: {registered:?}"
        );
    }

    #[test]
    fn zero_match_ids_filter_warns_loudly() {
        // An sboms config whose ids filter eliminates every kind-matched
        // artifact silently produces nothing — the stage must warn.
        let dist = tempfile::tempdir().expect("tempdir");
        let cfg = SbomConfig {
            ids: Some(vec!["no-such-id".to_string()]),
            ..per_archive_cfg()
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .tag("v1.0.0")
            .dist(dist.path().to_path_buf())
            .add_sbom(cfg)
            .build();
        let mut meta = HashMap::new();
        meta.insert("id".to_string(), "real-id".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: "app.tar.gz".to_string(),
            path: std::path::PathBuf::from("app.tar.gz"),
            target: None,
            crate_name: "app".to_string(),
            metadata: meta,
            size: None,
        });
        let capture = anodizer_core::log::LogCapture::new();
        ctx.with_log_capture(capture.clone());

        crate::SbomStage.run(&mut ctx).expect("sbom stage run");

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
    fn expected_sbom_assets_match_builtin_stage_registrations() {
        // Equivalence pin: run the REAL built-in SBOM generator over archives
        // in a temp dist and assert the derivation predicted exactly the Sbom
        // artifact names the stage registered — a SINGLE workspace document.
        let dist = tempfile::tempdir().expect("tempdir");
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .tag("v1.0.0")
            .dist(dist.path().to_path_buf())
            .add_sbom(per_archive_cfg())
            .build();
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(archive(
            "app-1.0.0-linux-amd64.tar.gz",
            "app",
            Some("x86_64-unknown-linux-gnu"),
        ));
        ctx.artifacts.add(archive(
            "app-1.0.0-darwin-arm64.tar.gz",
            "app",
            Some("aarch64-apple-darwin"),
        ));

        let predicted = expected_sbom_assets(&ctx, "app", None).expect("derivation");

        crate::SbomStage.run(&mut ctx).expect("sbom stage run");

        let mut registered: Vec<String> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Sbom)
            .map(|a| a.name.clone())
            .collect();
        registered.sort();
        registered.dedup();

        assert_eq!(
            predicted, registered,
            "config-derived SBOM expectations must equal what the stage registers"
        );
        assert_eq!(
            registered,
            vec!["app-1.0.0.cdx.json".to_string()],
            "built-in generator emits one workspace SBOM, not one per archive"
        );
    }

    fn installer(name: &str, crate_name: &str, format: &str) -> Artifact {
        let mut metadata = HashMap::new();
        metadata.insert(
            anodizer_core::artifact::FORMAT_META.to_string(),
            format.to_string(),
        );
        Artifact {
            kind: ArtifactKind::Installer,
            name: name.to_string(),
            path: std::path::PathBuf::from(name),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: crate_name.to_string(),
            metadata,
            size: None,
        }
    }

    #[test]
    fn a_directory_bundle_installer_yields_no_expectation_while_a_pkg_does() {
        // The stage skips a `.app` bundle (a directory never uploaded raw) on
        // both generator paths, so the gate must not demand a document for it.
        let cfg = SbomConfig {
            id: Some("inst".to_string()),
            cmd: Some("syft".to_string()),
            documents: Some(vec!["{{ .ArtifactName }}.cdx.json".to_string()]),
            artifacts: Some("installer".to_string()),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .add_sbom(cfg)
            .build();
        ctx.artifacts.add(installer(
            "app.app",
            "app",
            anodizer_core::artifact::FORMAT_APPBUNDLE,
        ));
        ctx.artifacts.add(installer("app-1.0.0.pkg", "app", "pkg"));
        assert_eq!(
            expected_sbom_assets(&ctx, "app", None).expect("derivation"),
            vec!["app-1.0.0.pkg.cdx.json".to_string()]
        );
    }

    /// A two-crate lockstep workspace whose release owner (`ws-cli`, the
    /// first crate with a `release:` block) is not `project_name`
    /// (`ws-core`), with one archive per crate in `dist`.
    fn lockstep_ctx(dist: &Path, cfg: SbomConfig) -> Context {
        use anodizer_core::config::{CrateConfig, ReleaseConfig};
        let release = || {
            Some(ReleaseConfig {
                github: Some(anodizer_core::config::ScmRepoConfig {
                    owner: "o".to_string(),
                    name: "r".to_string(),
                    token: None,
                }),
                ..Default::default()
            })
        };
        let mut ctx = TestContextBuilder::new()
            .project_name("ws-core")
            .tag("v1.0.0")
            .dist(dist.to_path_buf())
            .add_sbom(cfg)
            .crates(vec![
                CrateConfig {
                    name: "ws-cli".to_string(),
                    path: "crates/cli".to_string(),
                    release: release(),
                    ..Default::default()
                },
                CrateConfig {
                    name: "ws-core".to_string(),
                    path: "crates/core".to_string(),
                    release: release(),
                    ..Default::default()
                },
            ])
            .build();
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(archive(
            "ws-cli-1.0.0.tar.gz",
            "ws-cli",
            Some("x86_64-unknown-linux-gnu"),
        ));
        ctx.artifacts.add(archive(
            "ws-core-1.0.0.tar.gz",
            "ws-core",
            Some("x86_64-unknown-linux-gnu"),
        ));
        assert_eq!(ctx.config.release_owner_crate_name(), "ws-cli");
        ctx
    }

    /// The Sbom artifacts the stage registered under `crate_name`, sorted.
    fn registered_sboms_of(ctx: &Context, crate_name: &str) -> Vec<String> {
        let mut names: Vec<String> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::Sbom && a.crate_name == crate_name)
            .map(|a| a.name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Run the real stage on the lockstep context and hold, per crate, the
    /// derivation to the registrations under that crate.
    fn assert_lockstep_gate_matches_stage(cfg: SbomConfig, expect_owner_nonempty: bool) {
        let dist = tempfile::tempdir().expect("tempdir");
        let mut ctx = lockstep_ctx(dist.path(), cfg);
        let predicted_cli = expected_sbom_assets(&ctx, "ws-cli", None).expect("derivation");
        let predicted_core = expected_sbom_assets(&ctx, "ws-core", None).expect("derivation");

        crate::SbomStage.run(&mut ctx).expect("sbom stage run");

        assert_eq!(
            predicted_cli,
            registered_sboms_of(&ctx, "ws-cli"),
            "the release owner's derivation must equal the stage's registrations under it"
        );
        assert_eq!(
            predicted_core,
            registered_sboms_of(&ctx, "ws-core"),
            "the project crate's derivation must equal the stage's registrations under it"
        );
        if expect_owner_nonempty {
            assert!(
                !predicted_cli.is_empty(),
                "the owner's release expects the document"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn lockstep_external_typed_expectations_match_the_stage_per_crate() {
        // One document per archive, each registered under its archive's crate.
        assert_lockstep_gate_matches_stage(
            SbomConfig {
                id: Some("ext".to_string()),
                cmd: Some("sh".to_string()),
                args: Some(vec!["-c".to_string(), "echo x > \"$document\"".to_string()]),
                documents: Some(vec!["{{ .ArtifactName }}.spdx.json".to_string()]),
                artifacts: Some("archive".to_string()),
                ..Default::default()
            },
            true,
        );
    }

    #[test]
    #[cfg(unix)]
    fn lockstep_external_any_expectations_match_the_stage_per_crate() {
        // The whole-project document is registered under the release owner,
        // never under the project crate.
        assert_lockstep_gate_matches_stage(
            SbomConfig {
                id: Some("ext-any".to_string()),
                cmd: Some("sh".to_string()),
                args: Some(vec!["-c".to_string(), "echo x > \"$document\"".to_string()]),
                documents: Some(vec!["{{ .ArtifactName }}.spdx.json".to_string()]),
                artifacts: Some("any".to_string()),
                ..Default::default()
            },
            true,
        );
    }

    #[test]
    fn lockstep_builtin_expectations_match_the_stage_per_crate() {
        // The built-in generator's single workspace document carries the
        // project name and is registered under the release owner.
        assert_lockstep_gate_matches_stage(per_archive_cfg(), true);
    }
}
