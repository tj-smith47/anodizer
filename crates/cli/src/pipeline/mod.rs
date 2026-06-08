//! Release-pipeline orchestration for the `anodizer` CLI.
//!
//! This module is split into focused submodules:
//!
//! - [`config_loader`] — config discovery, format detection, `includes:`
//!   resolution, and post-load normalization.
//! - [`monorepo`] — monorepo path-prefix defaulting.
//! - [`builders`] — the `build_*_pipeline` constructors for each entry point.
//!
//! The [`Pipeline`] type and its sequential `run` loop live here; the
//! submodules' public surface is re-exported below so external callers keep
//! reaching items via `crate::pipeline::<item>`.

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::stage::Stage;
use anyhow::Result;
use colored::Colorize;

mod builders;
mod config_loader;
mod monorepo;

pub use anodizer_core::hooks::run_hooks;

pub(crate) use builders::build_publish_only_pipeline;
pub use builders::{
    build_announce_pipeline, build_merge_pipeline, build_publish_pipeline, build_release_pipeline,
    build_split_pipeline,
};
pub use config_loader::{find_config, find_config_with_logger, load_config, load_repo_config};

pub struct Pipeline {
    stages: Vec<Box<dyn Stage>>,
    /// Whether this pipeline is expected to have a compiled binary for
    /// every in-scope crate that configures a binary-requiring surface.
    ///
    /// Set only on the build-producing pipelines (full release, which
    /// runs `BuildStage`, and merge, which pre-loads the split shards'
    /// binaries before the pipeline runs). The publish-only / publish /
    /// announce pipelines leave it `false`: they rehydrate or never touch
    /// binary artifacts, so a missing binary there is not a build mistake
    /// the guard should fail on.
    expects_binaries: bool,
}

impl Pipeline {
    pub fn new() -> Self {
        Self {
            stages: vec![],
            expects_binaries: false,
        }
    }

    pub fn add(&mut self, stage: Box<dyn Stage>) {
        self.stages.push(stage);
    }

    /// Arm the binary-artifact guard for this pipeline. Call only on the
    /// build-producing pipelines (full release, merge); see
    /// [`anodizer_core::binary_artifact_guard`].
    pub(crate) fn expect_binaries(&mut self) {
        self.expects_binaries = true;
    }

    /// Returns the registered stage names in pipeline order. Used by the
    /// pipeline-construction tests to assert stage ordering invariants
    /// (e.g. blob runs before snapcraft-publish so the submitter gate
    /// sees blob's outcome via `ctx.publish_report`).
    #[cfg(test)]
    pub fn stage_names(&self) -> Vec<&str> {
        self.stages.iter().map(|s| s.name()).collect()
    }

    /// Run every registered stage in order; `emit_summary` always
    /// fires after the inner body returns, regardless of `Ok`/`Err`.
    ///
    /// `emit_summary` runs at the pipeline level (not inside
    /// `AnnounceStage::run`) so the end-of-pipeline status table and
    /// `--summary-json=<path>` write always fire — including when
    /// announce itself is operator-skipped via `--skip=announce`. The
    /// scope-guard shape (inner-fn returns the outcome, outer wrapper
    /// calls `emit_summary` then propagates) means the summary fires
    /// on Ok, Err, AND when the pipeline body short-circuits early
    /// via `?`.
    ///
    /// # Panics
    ///
    /// If a stage panics, the unwind happens BEFORE the
    /// `emit_summary` post-call runs, so a panicking pipeline body
    /// will skip the summary write. This is an accepted limitation
    /// — a stage that panics is a bug in the stage (or a panic from
    /// `unwrap`/`expect` we missed in review), not an operator
    /// error we can recover from. The release pipeline is built
    /// around `Result`-propagation; a panic means something the
    /// review failed to catch is wrong, and dropping `summary.json`
    /// in that scenario is bug-on-bug (the missing summary is a
    /// downstream symptom of the underlying panic, not a release
    /// gate). A `scopeguard::defer!` wrapper would close this
    /// window but adds a panic-safety abstraction layer the rest
    /// of the codebase doesn't use; the inner-fn shape mirrors the
    /// convention already established by
    /// `AnnounceStage::run` → `announce_body`.
    pub fn run(&self, ctx: &mut Context, log: &StageLogger) -> Result<()> {
        let outcome = self.run_inner(ctx, log);
        anodizer_stage_announce::emit_summary(ctx);
        outcome
    }

    /// Inner pipeline body. Lives separately so `Pipeline::run` can
    /// wrap it in the unconditional `emit_summary` post-call — see
    /// the audit reference at the top of `run`.
    fn run_inner(&self, ctx: &mut Context, log: &StageLogger) -> Result<()> {
        // Skip-stage validation runs at the CLI entry (`validate_skip_values`
        // in main.rs); the command never reaches this point with an unknown
        // value. No runtime warning is needed.

        // Stages that only make sense when binary artifacts exist.  When the
        // build stage produces no binaries (library-only crate), these stages
        // are skipped with a clear message instead of silently reporting ✓.
        const BINARY_DEPENDENT_STAGES: &[&str] = &[
            "upx",
            "archive",
            "makeself",
            "appimage",
            "nfpm",
            "snapcraft",
            "appbundle",
            "dmg",
            "msi",
            "pkg",
            "nsis",
            "flatpak",
            "notarize",
            "srpm",
        ];

        // Check if binaries already exist (merge mode loads artifacts before
        // the pipeline runs, so build stage never executes).
        let mut has_binaries = ctx.artifacts.all().iter().any(|a| {
            matches!(
                a.kind,
                anodizer_core::artifact::ArtifactKind::Binary
                    | anodizer_core::artifact::ArtifactKind::UploadableBinary
                    | anodizer_core::artifact::ArtifactKind::UniversalBinary
            )
        });

        // Whether `BuildStage` runs inside this pipeline. Drives where the
        // binary-artifact guard fires: merge pre-loads its binaries (no
        // build stage), so the guard runs up-front here; the full-release
        // and determinism-harness-child pipelines compile in-process, so
        // their guard runs immediately after the build stage completes.
        //
        // `--skip=build` registers the build stage but skips its body in the
        // loop below, so a registered-but-skipped build must count as "not
        // running": otherwise the up-front guard never fires (build appears
        // in-pipeline) AND the post-build guard never fires (the stage body
        // is `continue`d) — silently bypassing the guard. Honoring the skip
        // set routes such runs through the up-front guard, which validates
        // the prebuilt / pre-loaded binaries.
        let build_in_pipeline =
            self.stages.iter().any(|s| s.name() == "build") && !ctx.should_skip("build");

        // Merge-mode checkpoint: binaries are already loaded, so the
        // artifact set is final before the first stage runs.
        if self.expects_binaries && !build_in_pipeline {
            // Merge mode pre-loaded every crate's binaries and ran no build
            // stage, so there is no built-set to scope by — pass `None` to
            // check every in-scope crate.
            anodizer_core::binary_artifact_guard::check(
                &ctx.config,
                &ctx.artifacts,
                &ctx.options.selected_crates,
                None,
            )?;
        }

        for stage in &self.stages {
            let name = stage.name();
            // Operator-skipped stage: still open its section so the skip
            // note sits inside the stage's own group (one section per
            // stage in CI) rather than ungrouped after the last endgroup.
            if ctx.should_skip(name) {
                // No section: a skipped stage has no header to announce (the
                // header is deferred until a real body line, which a skip is
                // not), so emit the one neutral skip line at the current
                // (top) level — `• <name> skipped` reads flat, not nested
                // under a non-existent verb header. The stage name is the
                // line's subject (the per-line `[stage]` tag is gone).
                log.status(&format!("{name} {}", "skipped".yellow()));
                continue;
            }

            // After the build stage, check if any binary artifacts were produced.
            // Skip binary-dependent stages if not (library-only crate).
            // NOTE: This is a pipeline optimization, not a feature skip. Each stage
            // checks its own config internally; stages with no config return Ok(())
            // immediately. The strict_guard for "no binaries" lives inside the
            // individual stages (e.g., archive, upx) where it fires AFTER the stage
            // confirms it has work to do.
            if BINARY_DEPENDENT_STAGES.contains(&name) && !has_binaries {
                log.status(&format!("{name} {}", "skipped (no binaries)".yellow()));
                continue;
            }

            // Write metadata.json + artifacts.json before the release stage
            // so that include_meta can attach them to the GitHub release.
            // run_post_pipeline overwrites these with the final version later.
            if name == "release"
                && let Err(e) = write_pre_release_metadata(ctx)
            {
                log.warn(&format!("failed to write pre-release metadata: {}", e));
            }

            // One collapsible section per stage: `::group::<name>` under
            // GitHub Actions, a Cargo-style verb header locally. The guard
            // closes the section (`::endgroup::` / de-indent) when it drops
            // at the end of this loop iteration — on the normal path, on the
            // early `?` return below, and on any panic unwind — so the
            // section is always balanced without an explicit drop in either
            // arm.
            let _section = log.group(name);
            match stage.run(ctx) {
                Ok(()) => {
                    // After the build stage, record whether binaries were produced.
                    if name == "build" {
                        has_binaries = ctx.artifacts.all().iter().any(|a| {
                            matches!(
                                a.kind,
                                anodizer_core::artifact::ArtifactKind::Binary
                                    | anodizer_core::artifact::ArtifactKind::UploadableBinary
                                    | anodizer_core::artifact::ArtifactKind::UniversalBinary
                            )
                        });
                        // Build-producing checkpoint: the per-crate binary
                        // artifact set is final once the build stage finishes.
                        // Fail loud here so a crate that configures a
                        // binary-requiring surface but produced no binary
                        // aborts the release at build time rather than 20
                        // minutes later inside publish/docker.
                        if self.expects_binaries {
                            // Pass the set of crates the build stage actually
                            // built so a crate with no in-scope target in this
                            // shard is skipped, while a built-but-binary-less
                            // crate still fails.
                            anodizer_core::binary_artifact_guard::check(
                                &ctx.config,
                                &ctx.artifacts,
                                &ctx.options.selected_crates,
                                ctx.built_crate_names(),
                            )?;
                        }
                    }
                    // After the changelog stage completes, populate the ReleaseNotes
                    // template variable so subsequent stages can reference it.
                    if name == "changelog" {
                        ctx.populate_release_notes_var();
                    }
                }
                Err(e) => {
                    // The message names the failing stage; the section header
                    // already scopes it inside `::group::<name>`.
                    log.error(&format!("{name} failed: {e}"));
                    return Err(e);
                }
            }
        }

        // End-of-pipeline skip summary. Stages (sign, docker-sign, publisher)
        // record intentional per-sub-config skips via
        // `ctx.remember_skip(...)`; before this hook the skips were emitted
        // at verbose level and lost in the final "✓ done" output.
        let skips = ctx.skip_memento.drain();
        if !skips.is_empty() {
            let noun = if skips.len() == 1 {
                "intentional skip"
            } else {
                "intentional skips"
            };
            log.status(&format!("{} {}:", skips.len(), noun.yellow()));
            for ev in &skips {
                log.status(&format!(
                    "  {} [{}] {} — {}",
                    "\u{21b3}".yellow(),
                    ev.stage.bold(),
                    ev.label,
                    ev.reason
                ));
            }
        }
        Ok(())
    }
}

/// Write preliminary metadata.json and artifacts.json before the release
/// stage so that `include_meta: true` can attach them to the GitHub release.
/// `run_post_pipeline` overwrites these with the final version afterward.
fn write_pre_release_metadata(ctx: &mut anodizer_core::context::Context) -> anyhow::Result<()> {
    let dist = &ctx.config.dist;
    std::fs::create_dir_all(dist)?;

    let tag = ctx.template_vars().get("Tag").cloned().unwrap_or_default();
    let version = ctx.version();
    let commit = ctx
        .template_vars()
        .get("FullCommit")
        .cloned()
        .unwrap_or_default();

    let metadata = serde_json::json!({
        "project_name": ctx.config.project_name,
        "tag": tag,
        "version": version,
        "commit": commit,
    });
    std::fs::write(
        dist.join("metadata.json"),
        serde_json::to_string_pretty(&metadata)?,
    )?;

    let artifacts_json = ctx.artifacts.to_artifacts_json()?;
    std::fs::write(
        dist.join("artifacts.json"),
        serde_json::to_string_pretty(&artifacts_json)?,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{Config, CrateConfig, DockerV2Config};
    use anodizer_core::context::{Context, ContextOptions};
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// No-op stage standing in for `BuildStage`: it shares the `"build"`
    /// name (so the pipeline's skip-set + guard plumbing treats it as the
    /// build stage) but produces no artifacts, mimicking a misconfigured
    /// build that compiles nothing.
    struct NoopBuildStage;
    impl Stage for NoopBuildStage {
        fn name(&self) -> &str {
            "build"
        }
        fn run(&self, _ctx: &mut Context) -> Result<()> {
            Ok(())
        }
    }

    fn binary_surface_config() -> Config {
        Config {
            crates: vec![CrateConfig {
                name: "svc".to_string(),
                dockers_v2: Some(vec![DockerV2Config::default()]),
                ..CrateConfig::default()
            }],
            ..Config::default()
        }
    }

    fn source_artifact() -> Artifact {
        Artifact {
            kind: ArtifactKind::SourceArchive,
            path: PathBuf::from("dist/svc.tar.gz"),
            name: "svc.tar.gz".to_string(),
            target: None,
            crate_name: "svc".to_string(),
            metadata: HashMap::new(),
            size: None,
        }
    }

    /// `--skip=build` must NOT disarm the binary-presence guard: the crate
    /// configures a binary-requiring surface (docker_v2) but only a source
    /// archive is present, so the up-front guard must fire rather than the
    /// pipeline silently proceeding with a source-only dist.
    #[test]
    fn skip_build_still_runs_binary_presence_guard() {
        let mut p = Pipeline::new();
        p.add(Box::new(NoopBuildStage));
        p.expect_binaries();

        let opts = ContextOptions {
            skip_stages: vec!["build".to_string()],
            ..Default::default()
        };
        let mut ctx = Context::new(binary_surface_config(), opts);
        ctx.artifacts.add(source_artifact());

        let log = ctx.logger("pipeline-test");
        let err = p
            .run(&mut ctx, &log)
            .expect_err("guard must fire with --skip=build and no binary");
        let msg = err.to_string();
        assert!(msg.contains("crate 'svc'"), "{msg}");
        assert!(msg.contains("no binary artifacts"), "{msg}");
    }

    /// Control: with a real prebuilt binary present, `--skip=build` passes
    /// the guard cleanly — the fix validates binaries, it does not blanket-
    /// fail every skip-build run.
    #[test]
    fn skip_build_passes_guard_when_prebuilt_binary_present() {
        let mut p = Pipeline::new();
        p.add(Box::new(NoopBuildStage));
        p.expect_binaries();

        let opts = ContextOptions {
            skip_stages: vec!["build".to_string()],
            ..Default::default()
        };
        let mut ctx = Context::new(binary_surface_config(), opts);
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: PathBuf::from("dist/svc"),
            name: "svc".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "svc".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let log = ctx.logger("pipeline-test");
        p.run(&mut ctx, &log)
            .expect("prebuilt binary satisfies the guard under --skip=build");
    }
}
