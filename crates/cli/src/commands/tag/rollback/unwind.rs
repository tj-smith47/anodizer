//! Publisher-unwind step for `anodize tag rollback`.
//!
//! Withdrawing a release has two halves. The git half — revert the bump
//! commit, delete the tags, delete the tag's GitHub release — lives in
//! [`super::run()`]. This module is the other half: re-invoking each
//! `Publisher`'s `rollback()` against the state a prior release run
//! persisted, so a deliberate withdrawal also closes the tap PRs, deletes
//! the mirrored blobs, and yanks what a one-way door left behind.
//!
//! The operator supplies nothing. A release run writes its state to
//! `<dist>/run-<id>/report.json` with `run_id` == the tag it cut, so the
//! tags this rollback already resolved from the target SHA name their own
//! run dirs. Both dist layouts are swept: `<dist>/run-<tag>/` for
//! single-crate and lockstep configs, `<dist>/<crate>/run-<tag>/` for
//! per-crate workspaces.

use anodizer_core::config::Config;
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

use super::types::RollbackOpts;

/// One prior release run whose recorded publisher state this rollback
/// withdraws.
struct UnwindTarget {
    /// The tag being rolled back — also the `run_id` the release pipeline
    /// derived for the run dir (`derive_run_id` prefers the tag).
    tag: String,
    /// Dist root the run dir hangs off: `<dist>` for single-crate and
    /// lockstep configs, `<dist>/<crate>` for per-crate workspaces.
    dist: PathBuf,
}

/// Re-invoke publisher rollback for every prior run recorded against
/// `tags`, before the tags (and their GitHub releases) are destroyed.
///
/// A run dir with no persisted state warns and is skipped — the common
/// shape for a checkout that never ran the release. Unreadable state is a
/// hard error under normal operation (the withdrawal cannot be completed,
/// so the tag that documents the published state must not be destroyed)
/// and a warning under `--force`, which already means "proceed regardless
/// of what the evidence says".
///
/// `--dry-run` prints the publishers each run would withdraw and touches
/// nothing.
pub(super) fn unwind_published_state(
    cwd: &Path,
    repo_config: &Config,
    tags: &[String],
    opts: &RollbackOpts,
    log: &StageLogger,
) -> Result<()> {
    let dist_root = super::guard::resolve_dist_dir(cwd, repo_config);
    let targets = discover_unwind_targets(&dist_root, tags);
    if targets.is_empty() {
        // Warn, not verbose: the tag and its GitHub release are about to be
        // destroyed, and a miss here means whatever that run published stays
        // live with nothing left pointing at it. The benign shape (a checkout
        // that never ran the release) is worth the line.
        log.warn(&format!(
            "no run dir found under {} for {} — the publisher unwind is being SKIPPED, so any \
             published state from a prior run will be left in place. Roll back from the \
             checkout that ran the release (the one whose dist/ holds run-<tag>/), or accept \
             that this is a tag-only rollback.",
            dist_root.display(),
            tags.join(", ")
        ));
        return Ok(());
    }

    for target in targets {
        let mut ctx = unwind_context(repo_config, &target, opts);
        if !anodizer_stage_publish::rollback::prior_state_exists(&ctx, &target.tag) {
            log.warn(&format!(
                "no publisher report under {} — skipping the publisher unwind for {} \
                 (the release run that wrote this directory recorded no per-publisher state)",
                target.dist.display(),
                target.tag
            ));
            continue;
        }

        if opts.dry_run {
            let planned =
                anodizer_stage_publish::rollback::planned_rollback_names(&ctx, &target.tag)
                    .with_context(|| {
                        format!(
                            "could not read the recorded publisher state for {}",
                            target.tag
                        )
                    })?;
            if planned.is_empty() {
                log.status(&format!(
                    "(dry-run) no publisher state to withdraw for {}",
                    target.tag
                ));
            } else {
                log.status(&format!(
                    "(dry-run) would withdraw {} publisher(s) for {}: {}",
                    planned.len(),
                    target.tag,
                    planned.join(", ")
                ));
            }
            continue;
        }

        log.status(&format!("unwinding publishers recorded for {}", target.tag));
        match anodizer_stage_publish::rollback::run(&mut ctx, &target.tag) {
            Ok(_) => {}
            Err(e) if opts.force => log.warn(&format!(
                "could not unwind publishers for {} ({e:#}); proceeding — --force",
                target.tag
            )),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "refusing to roll back {} — its recorded publisher state under {} could \
                         not be read, so the published state cannot be withdrawn. Destroying the \
                         tag now would orphan whatever that run published. Fix or delete the \
                         file, or re-run with --force to withdraw the tag anyway.",
                        target.tag,
                        target.dist.display()
                    )
                });
            }
        }
    }
    Ok(())
}

/// Find the run dirs a prior release wrote for `tags`, across both dist
/// layouts. Returns targets in a deterministic order (dist root before
/// per-crate subdirs, tags in the caller's order) so the unwind sequence
/// is reproducible across runs.
fn discover_unwind_targets(dist_root: &Path, tags: &[String]) -> Vec<UnwindTarget> {
    let mut roots = vec![dist_root.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(dist_root) {
        let mut subdirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        subdirs.sort();
        roots.extend(subdirs);
    }

    let mut out = Vec::new();
    for root in roots {
        for tag in tags {
            let run_dir = root.join(format!("{}{tag}", anodizer_core::dist::RUN_DIR_PREFIX));
            if run_dir.is_dir() {
                out.push(UnwindTarget {
                    tag: tag.clone(),
                    dist: root.clone(),
                });
            }
        }
    }
    out
}

/// Build the `Context` the unwind engine drives one target with.
///
/// `dist` is re-anchored onto the run dir's parent so the engine reads and
/// rewrites the right `run-<tag>/` in a per-crate workspace. `Tag` (and
/// `Version`, when the tag carries a parseable one) are seeded from the tag
/// being withdrawn so `on_rollback` hooks render the release they are
/// firing for rather than an empty string.
fn unwind_context(repo_config: &Config, target: &UnwindTarget, opts: &RollbackOpts) -> Context {
    let mut config = repo_config.clone();
    config.dist = target.dist.clone();
    let mut ctx = Context::new(
        config,
        ContextOptions {
            verbose: opts.verbose,
            debug: opts.debug,
            quiet: opts.quiet,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Tag", &target.tag);
    if let Some((_, version)) = anodizer_core::git::split_tag_family(&target.tag) {
        let version = version.version_string();
        ctx.template_vars_mut().set("Version", &version);
        ctx.template_vars_mut().set("RawVersion", &version);
    }
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_report(dir: &Path, publisher: &str) {
        std::fs::create_dir_all(dir).expect("create run dir");
        let report = format!(
            r#"{{
  "results": [
    {{
      "name": "{publisher}",
      "group": "Manager",
      "required": true,
      "outcome": "Succeeded",
      "evidence": {{
        "schema_version": 1,
        "publisher": "{publisher}",
        "primary_ref": null,
        "artifact_paths": [],
        "nondeterministic": null,
        "extra": {{}}
      }}
    }}
  ],
  "submitter_gated": false,
  "announce_gated": false
}}"#
        );
        std::fs::write(dir.join("report.json"), report).expect("write report");
    }

    fn opts(dry_run: bool, force: bool) -> RollbackOpts {
        RollbackOpts {
            sha: None,
            dry_run,
            no_push: false,
            force,
            scope: super::super::types::Scope::All,
            mode: super::super::types::Mode::Revert,
            branch: None,
            verbose: false,
            debug: false,
            quiet: true,
        }
    }

    /// Both dist layouts are swept from the tag alone: the flat
    /// `<dist>/run-<tag>/` and the per-crate `<dist>/<crate>/run-<tag>/`.
    /// A discovery that only looked at the dist root would silently skip
    /// every per-crate workspace's published state.
    #[test]
    fn discovery_finds_both_dist_layouts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(dist.join("run-v1.2.3")).expect("flat run dir");
        std::fs::create_dir_all(dist.join("crate-a").join("run-v1.2.3")).expect("per-crate run");
        // Distractors: a different tag, and a non-run directory.
        std::fs::create_dir_all(dist.join("run-v9.9.9")).expect("other tag");
        std::fs::create_dir_all(dist.join("not-a-run")).expect("non-run dir");

        let found = discover_unwind_targets(&dist, &["v1.2.3".to_string()]);
        let dirs: Vec<PathBuf> = found.iter().map(|t| t.dist.clone()).collect();
        assert_eq!(
            dirs,
            vec![dist.clone(), dist.join("crate-a")],
            "both layouts must be discovered, dist root first"
        );
        assert!(
            found.iter().all(|t| t.tag == "v1.2.3"),
            "every target must carry the tag it was discovered for"
        );
    }

    /// A missed run dir must be LOUD. The tag and its GitHub release are
    /// about to be destroyed, so "publisher state was not withdrawn" is the
    /// operator's only chance to notice that whatever the run published is
    /// now orphaned — a verbose-only line hides it behind a flag nobody
    /// passes to a rollback.
    #[test]
    fn a_missing_run_dir_warns_rather_than_whispering() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).expect("dist");

        let (log, capture) =
            StageLogger::with_capture("test", anodizer_core::log::Verbosity::Normal);
        unwind_published_state(
            tmp.path(),
            &Config {
                dist: dist.clone(),
                ..Default::default()
            },
            &["v1.2.3".to_string()],
            &opts(false, false),
            &log,
        )
        .expect("a missing run dir must not fail the rollback");

        let warns = capture.warn_messages();
        assert!(
            warns
                .iter()
                .any(|m| m.contains("publisher unwind is being SKIPPED")),
            "the miss must be a warn, got {warns:?} (all: {:?})",
            capture.all_messages()
        );
    }

    /// A tag with no run dir at all yields nothing — a checkout that never
    /// ran the release must not be treated as having state to withdraw.
    #[test]
    fn discovery_is_empty_without_a_matching_run_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(dist.join("run-v1.0.0")).expect("run dir");
        assert!(discover_unwind_targets(&dist, &["v2.0.0".to_string()]).is_empty());
    }

    /// `--dry-run` must preview the withdrawal and mutate nothing: no
    /// `rollback.json` may appear, and the untouched `report.json` must
    /// still be byte-identical afterwards.
    #[test]
    fn dry_run_previews_and_writes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path().join("dist");
        let run_dir = dist.join("run-v1.2.3");
        write_report(&run_dir, "orphan-mgr");
        let before = std::fs::read(run_dir.join("report.json")).expect("read report");

        let config = Config {
            dist: dist.clone(),
            ..Default::default()
        };
        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        unwind_published_state(
            tmp.path(),
            &config,
            &["v1.2.3".to_string()],
            &opts(true, false),
            &log,
        )
        .expect("dry-run unwind must succeed");

        assert!(
            !run_dir.join("rollback.json").exists(),
            "dry-run must not write rollback.json"
        );
        assert_eq!(
            before,
            std::fs::read(run_dir.join("report.json")).expect("re-read report"),
            "dry-run must leave report.json untouched"
        );
    }

    /// A real (non-dry-run) unwind drives the engine and persists its
    /// verdict. The fixture names a publisher no registry carries, so the
    /// row lands as `RollbackFailed(publisher not found ...)` — the
    /// documented diagnostic, and proof the engine actually ran.
    #[test]
    fn unwind_dispatches_and_persists_rollback_state() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path().join("dist");
        let run_dir = dist.join("run-v1.2.3");
        write_report(&run_dir, "orphan-mgr");

        let config = Config {
            dist: dist.clone(),
            ..Default::default()
        };
        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        unwind_published_state(
            tmp.path(),
            &config,
            &["v1.2.3".to_string()],
            &opts(false, false),
            &log,
        )
        .expect("unwind must succeed even when a recorded publisher is unknown");

        let state =
            std::fs::read_to_string(run_dir.join("rollback.json")).expect("rollback.json written");
        assert!(
            state.contains("RollbackFailed") && state.contains("not found in current registry"),
            "the engine's verdict must be persisted, got:\n{state}"
        );
    }

    /// Unreadable recorded state is a REFUSAL, not a shrug: completing the
    /// git half while the publisher half is unknown would orphan whatever
    /// that run published. `--force` downgrades it to a warning.
    #[test]
    fn unreadable_state_refuses_unless_forced() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path().join("dist");
        let run_dir = dist.join("run-v1.2.3");
        std::fs::create_dir_all(&run_dir).expect("run dir");
        std::fs::write(run_dir.join("report.json"), "{ not json").expect("write corrupt report");

        let config = Config {
            dist: dist.clone(),
            ..Default::default()
        };
        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        let err = unwind_published_state(
            tmp.path(),
            &config,
            &["v1.2.3".to_string()],
            &opts(false, false),
            &log,
        )
        .expect_err("corrupt recorded state must refuse the rollback");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("refusing to roll back v1.2.3"),
            "the refusal must name the tag it protected, got: {msg}"
        );

        unwind_published_state(
            tmp.path(),
            &config,
            &["v1.2.3".to_string()],
            &opts(false, true),
            &log,
        )
        .expect("--force must proceed past unreadable state");
    }

    /// A run dir that exists but recorded no publisher state is skipped,
    /// not failed — the shape left by a run that died before the publish
    /// stage wrote anything.
    #[test]
    fn run_dir_without_a_report_is_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(dist.join("run-v1.2.3")).expect("empty run dir");

        let config = Config {
            dist: dist.clone(),
            ..Default::default()
        };
        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        unwind_published_state(
            tmp.path(),
            &config,
            &["v1.2.3".to_string()],
            &opts(false, false),
            &log,
        )
        .expect("a run dir with no recorded state must not fail the rollback");
    }

    /// The engine's `Context` is re-anchored onto the run dir's parent so a
    /// per-crate workspace's state is read and rewritten in place, and the
    /// tag seeds the template vars `on_rollback` hooks render.
    #[test]
    fn context_anchors_dist_and_seeds_tag_vars() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let per_crate = tmp.path().join("dist").join("crate-a");
        let target = UnwindTarget {
            tag: "crate-a-v2.5.1".to_string(),
            dist: per_crate.clone(),
        };
        let ctx = unwind_context(&Config::default(), &target, &opts(false, false));
        assert_eq!(ctx.config.dist, per_crate);
        assert_eq!(
            ctx.template_vars().get("Tag").map(String::as_str),
            Some("crate-a-v2.5.1")
        );
        assert_eq!(
            ctx.template_vars().get("Version").map(String::as_str),
            Some("2.5.1")
        );
    }
}
