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
/// Returns a sorted, de-duplicated list of expected asset basenames. SBOM
/// artifacts are registered under the project name (see `run_sbom` /
/// `run_sbom_builtin`), so in workspace modes the expectations attach to the
/// crate whose name matches the project and every other crate gets none —
/// mirroring exactly which crate's release the upload path attaches SBOMs to.
pub fn expected_sbom_assets(ctx: &Context, crate_name: &str) -> Result<Vec<String>> {
    if ctx.should_skip("sbom") {
        return Ok(Vec::new());
    }
    if crate_name != ctx.config.project_name {
        return Ok(Vec::new());
    }
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
            if use_builtin {
                let (_, extension) = builtin_format_and_extension(&documents);
                expected.push(format!("{project_name}-{version}.{extension}"));
            } else {
                for doc in &documents {
                    let rendered = ctx.render_template(doc).with_context(|| {
                        format!("sbom[{id}]: failed to render document template '{doc}'")
                    })?;
                    push_predictable_basename(&mut expected, &rendered);
                }
            }
            continue;
        }

        let Some(doc_tpl) = documents.first() else {
            continue;
        };

        let matching: Vec<&Artifact> = if artifacts_type == "binary" {
            ctx.artifacts
                .binary_like_dedup()
                .into_iter()
                .filter(|a| matches_id_filter(a, cfg.ids.as_deref()))
                .collect()
        } else {
            let kind = typed_artifact_kind(artifacts_type, id)?;
            ctx.artifacts
                .all()
                .iter()
                .filter(|a| a.kind == kind)
                .filter(|a| matches_id_filter(a, cfg.ids.as_deref()))
                .collect()
        };

        for artifact in matching {
            let rendered = render_document_for_artifact(ctx, doc_tpl, artifact, id)?;
            push_predictable_basename(&mut expected, &rendered);
        }
    }

    expected.sort();
    expected.dedup();
    Ok(expected)
}

/// Render a `documents:` template for one matched artifact with the same
/// per-artifact template variables the SBOM stage binds (`ArtifactName`,
/// `ArtifactExt`, `ArtifactID`, `Os`/`Arch`/`Target`), without mutating the
/// shared context — the derivation runs read-only at verify time.
fn render_document_for_artifact(
    ctx: &Context,
    doc_tpl: &str,
    artifact: &Artifact,
    id: &str,
) -> Result<String> {
    let artifact_name = artifact
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact");

    let mut vars = ctx.template_vars().clone();
    vars.set("ArtifactName", artifact_name);
    vars.set(
        "ArtifactExt",
        artifact
            .metadata
            .get("ext")
            .filter(|s| !s.is_empty())
            .map(|s| s.as_str())
            .unwrap_or_else(|| anodizer_core::template::extract_artifact_ext(artifact_name)),
    );
    vars.set(
        "ArtifactID",
        artifact
            .metadata
            .get("id")
            .map(|s| s.as_str())
            .unwrap_or(""),
    );
    let target = artifact
        .target
        .as_deref()
        .or_else(|| artifact.metadata.get("target").map(|s| s.as_str()));
    if let Some(target) = target {
        let (os, arch) = anodizer_core::target::map_target(target);
        vars.set("Os", &os);
        vars.set("Arch", &arch);
        vars.set("Target", target);
    }

    anodizer_core::template::render(doc_tpl, &vars)
        .with_context(|| format!("sbom[{id}]: failed to render document template '{doc_tpl}'"))
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
    fn builtin_per_archive_expectations() {
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .add_sbom(per_archive_cfg())
            .build();
        ctx.artifacts
            .add(archive("app-1.0-linux-amd64.tar.gz", "app", None));
        ctx.artifacts
            .add(archive("app-1.0-darwin-arm64.tar.gz", "app", None));

        let expected = expected_sbom_assets(&ctx, "app").expect("derivation");
        assert_eq!(
            expected,
            vec![
                "app-1.0-darwin-arm64.tar.gz.cdx.json".to_string(),
                "app-1.0-linux-amd64.tar.gz.cdx.json".to_string(),
            ]
        );
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
        let expected = expected_sbom_assets(&ctx, "app").expect("derivation");
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
        let expected = expected_sbom_assets(&ctx, "app").expect("derivation");
        assert!(expected.is_empty());
    }

    #[test]
    fn non_project_crate_gets_no_sbom_expectations() {
        // SBOM artifacts are registered under the project name; in workspace
        // modes other crates' releases never carry them.
        let mut ctx = TestContextBuilder::new()
            .project_name("app")
            .add_sbom(per_archive_cfg())
            .build();
        ctx.artifacts
            .add(archive("other.tar.gz", "other-crate", None));
        let expected = expected_sbom_assets(&ctx, "other-crate").expect("derivation");
        assert!(expected.is_empty());
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
        let expected = expected_sbom_assets(&ctx, "app").expect("derivation");
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
        let expected = expected_sbom_assets(&ctx, "app").expect("derivation");
        assert_eq!(expected, vec!["app-1.2.3.cdx.json".to_string()]);
    }

    #[test]
    fn expected_sbom_assets_match_builtin_stage_registrations() {
        // Equivalence pin: run the REAL built-in SBOM generator over archives
        // in a temp dist and assert the derivation predicted exactly the Sbom
        // artifact names the stage registered.
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

        let predicted = expected_sbom_assets(&ctx, "app").expect("derivation");

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
    }
}
