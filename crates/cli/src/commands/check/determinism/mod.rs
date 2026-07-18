//! `anodize check determinism` CLI dispatcher.
//!
//! Body of the harness lives in [`crate::determinism_harness`]; this
//! module is responsible for:
//!
//! 1. Resolving the SOURCE_DATE_EPOCH from either the snapshot resolver
//!    (`--snapshot`) or the HEAD commit timestamp (default).
//! 2. Picking up the compile-time allow-list seeded by
//!    [`anodizer_core::DeterminismState::seed_from_commit`].
//! 3. Choosing the report path (CLI override → `dist/run-<commit_short>/determinism.json`).
//! 4. Invoking [`crate::determinism_harness::Harness::run`].
//! 5. Writing the report JSON and exiting non-zero on drift.

use std::collections::BTreeMap;

use crate::determinism_harness::{Harness, ResolvedDockerConfig, StageId, installer_stages};
use anodizer_cli::CheckDeterminismArgs;
use anodizer_core::{
    AllowList, AllowListEntry, DeterminismState,
    git::{head_commit_hash_in, head_commit_timestamp_in, head_is_at_tag, resolve_snapshot_sde},
    log::{StageLogger, Verbosity, render_error, render_note},
};
use anyhow::{Context, Result};
use strum::IntoEnumIterator;

pub fn run(args: CheckDeterminismArgs, verbose: bool, debug: bool, quiet: bool) -> Result<()> {
    let verbosity = Verbosity::from_flags(quiet, verbose, debug);

    // `--inject-drift` is a test-only flag gated by
    // `ANODIZE_TEST_HARNESS=1`. The flag is hidden from `--help`, so the
    // only way for an operator to trip the rejection branch is to type
    // it deliberately; the hard error keeps the surface from being
    // exercised accidentally on production releases. Gated FIRST — a
    // forbidden hidden flag must reject independent of any other arg's
    // value, so the operator gets the actionable "this flag is gated"
    // error rather than a complaint about `--runs`.
    let inject_drift = if std::env::var("ANODIZE_TEST_HARNESS").as_deref() == Ok("1") {
        args.inject_drift.clone()
    } else if args.inject_drift.is_some() {
        anyhow::bail!("--inject-drift requires ANODIZE_TEST_HARNESS=1 (test-harness gated flag)");
    } else {
        None
    };

    // A determinism check needs at least two rebuilds to compare: with `--runs=1`
    // every artifact's hash list is trivially self-equal, and `--runs=0` compares
    // nothing — both would print "N/N byte-identical" and exit 0 while verifying
    // nothing. Reject rather than silently clamp so the operator's intent isn't
    // rewritten under them.
    if args.runs < 2 {
        anyhow::bail!(
            "check determinism: --runs must be >= 2 (a determinism check compares at least two \
             rebuilds); got {}",
            args.runs
        );
    }

    let repo_root = std::env::current_dir().context("resolving repo root")?;

    // SDE source — snapshot resolver under --snapshot (handles dirty
    // tree); HEAD commit timestamp otherwise. Both routes converge on
    // an i64 "seconds since UNIX epoch" value.
    let sde = if args.snapshot {
        resolve_snapshot_sde(&repo_root)?
    } else {
        head_commit_timestamp_in(&repo_root)?
    };

    let commit = head_commit_hash_in(&repo_root)?;

    // One config load for every best-effort probe below (the stage-default
    // intersection here, signature allow-list, all-prebuilt short-circuit,
    // docker-backend hint). Loading once means the load-time legacy-alias
    // warnings print once per invocation instead of once per probe.
    // Best-effort: a missing/unparseable config yields `None` and the real
    // error surfaces from the pipeline itself.
    // `apply_defaults` materializes `defaults:` producer blocks onto crates, so
    // every consumer below — the configured-producer detection AND the docker /
    // msi / signing / prebuilt probes — reads the SAME resolved view the
    // pipeline's stage gates see. A raw config would miss a producer declared
    // only under `defaults:`, silently diverging the determinism stage set (and
    // its tool probes) from what the pipeline actually runs.
    let repo_config = crate::pipeline::load_repo_config(&repo_root)
        .ok()
        .map(|mut c| {
            anodizer_core::defaults_merge::apply_defaults(&mut c);
            c
        });

    // Absent / empty `--stages` resolves to the host-OS partition INTERSECTED
    // with the producers this config configures; an explicit selection passes
    // through unchanged.
    let stages = resolve_stages(args.stages.as_deref(), repo_config.as_ref())
        .map_err(|e| anyhow::anyhow!(e))?;
    // The operator's EXPLICIT selection (empty when the set came from the host
    // default). The harness hard-fails a missing tool only for explicitly typed
    // stages; host-default stages warn-skip. `--stages=""` (all-empty tokens)
    // resolves to the host default, so it is treated as non-explicit too.
    let explicit_stages = if is_explicit_stage_selection(args.stages.as_deref()) {
        stages.clone()
    } else {
        Vec::new()
    };
    let targets = parse_targets(args.targets.as_deref()).map_err(|e| anyhow::anyhow!(e))?;

    let report_path = args.report.clone().unwrap_or_else(|| {
        repo_root.join(format!(
            "dist/run-{}/determinism.json",
            commit_short(&commit)
        ))
    });

    // `--preserve-dist=<path>` may be relative; resolve against the
    // repo root so the harness has an absolute target. The repo_root
    // is `current_dir`, so a relative `--preserve-dist=./preserved-dist`
    // lands at `<cwd>/preserved-dist` — what a CI step expects when
    // passing the flag verbatim.
    //
    // The per-crate subdir append (`<base>/<crate>`) for multi-crate
    // workspaces is applied internally by the harness from
    // `crate_name` — doing it again here would double-prefix to
    // `<base>/<crate>/<crate>` and break the
    // upload/merge/`detect_dist_layout` flow.
    let preserve_dist = args.preserve_dist.as_ref().map(|p| {
        if p.is_absolute() {
            p.clone()
        } else {
            repo_root.join(p)
        }
    });

    // The harness emits its own per-stage warnings/notes through the shared
    // logger; this dispatcher owns the run's section (header + the full
    // run-configuration summary as aligned `kv` rows, one level beneath it).
    // It is the single printer of these parameters — the child release
    // subprocess and any wrapping CI script must not repeat them.
    let log = StageLogger::new("check", verbosity);
    let _section = log.group("check-determinism");
    emit_run_summary(
        &log,
        targets.as_deref(),
        &stages,
        args.runs,
        preserve_dist.as_deref(),
        args.crate_name.as_deref(),
    );

    // Submitter moderation-queue advisories are verbose-only; emit them once
    // here off the single load (hidden at the default log level).
    if let Some(ref cfg) = repo_config {
        crate::pipeline::emit_config_advisories(cfg, &log);
        // The harness resolves stages/producers through the deduped crate
        // universe, which silently DROPS a shadowed same-name crate — warn
        // here (as the publish stage does) so the dedup is never invisible
        // to an operator whose colliding crate simply isn't checked.
        for w in cfg.crate_universe_collision_warnings() {
            log.warn(&w);
        }
    }

    // Seed the compile-time allow-list from the centralized
    // DeterminismState (single source of truth); the runtime allow-list
    // is empty here because the harness is invoked outside the
    // `release` pipeline that would have populated it.
    let state = DeterminismState::seed_from_commit(sde)
        .context("seeding determinism state from HEAD commit timestamp")?;
    let mut allowlist = AllowList {
        compile_time: state
            .compile_time_allowlist
            .iter()
            .map(|(n, r)| AllowListEntry {
                artifact: n.clone(),
                reason: r.clone(),
            })
            .collect(),
        runtime: Vec::new(),
    };
    // Signature artifacts (cosign bundles, gpg sigs) are non-reproducible
    // by nature; derive their suffixes from the project's `signs:` /
    // `binary_signs:` templates so the harness excludes them from drift
    // regardless of the user's chosen `signature:` naming scheme (e.g.
    // cfgd's `{{ .Artifact }}.cosign.bundle`, which would otherwise fall
    // through `infer_stage_from_path` to `unknown` and count as drift).
    allowlist.runtime.extend(
        repo_config
            .as_ref()
            .map(signature_allowlist_entries_from_config)
            .unwrap_or_default(),
    );

    // Fallback only — production runs always have a sibling metadata.json
    // that wins. A missing or malformed one would otherwise emit anodizer's
    // own version into `context.json:version`, which third-party consumers
    // would then publish as their own release version.
    let version_hint =
        read_project_version(&repo_root).unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    let child_snapshot =
        resolve_child_snapshot(args.snapshot, args.no_snapshot, head_is_at_tag(&repo_root)?);

    // All-prebuilt short-circuit: when every `builds[]` entry uses
    // `builder: prebuilt`, no target compiles and the harness has nothing
    // to rebuild. Re-running the import twice would just stat the same
    // staged path twice; the bytes are guaranteed identical by
    // construction. Emit a status line and return without spawning the
    // harness so CI doesn't churn on an empty matrix.
    //
    // Mixed configs (some prebuilt + some cargo) still run the harness —
    // the cargo targets need the rebuild, and the prebuilt artifacts
    // appear in both runs at the same staged path with identical bytes
    // (so they fall through the diff cleanly).
    if repo_config
        .as_ref()
        .map(anodizer_core::config::all_builds_prebuilt)
        .unwrap_or(false)
    {
        eprintln!(
            "{}",
            render_note(
                "determinism harness skipped: no buildable targets (all builds use `builder: prebuilt`)"
            )
        );
        return Ok(());
    }

    // Inspect the project's docker_v2 configs for a `use: podman` opt-in.
    // The harness's docker stage shells out to `docker buildx`, which is
    // not compatible with podman's flag set — propagate the hint so the
    // harness can skip the docker stage with a clear message instead of
    // probing reproducibility against a binary the operator will never
    // ship. Failure to load the config (missing file, parse error) is
    // soft: the docker stage falls through to its existing buildx path.
    let docker_backend_hint = repo_config.as_ref().and_then(detect_docker_backend_hint);

    // Resolve the crate-under-test's `dockers_v2` entries (rendered dockerfile
    // path + extra_files + build_args) so the harness docker path builds the
    // SAME image(s) the production `docker` stage would — never a stray
    // repo-root `Dockerfile`. Rendered here (the dispatcher owns a `Context`);
    // the harness receives plain data. `docker_declared` records whether the
    // crate configures ANY docker image, independent of render outcome, so the
    // harness can distinguish an unconfigured crate (clean skip) from a
    // declared-but-unresolved one (hard error under an explicit request).
    //
    // Only resolved when `docker` is in the stage set (the harness never runs
    // the docker stage otherwise), which also avoids the git/env setup cost and
    // side effects on unrelated runs. The resolve outcome + operator intent are
    // forked by `classify_docker_stage_state` (which also reconciles
    // `docker_declared` with an errored host-default resolve — see its doc).
    let docker_declared_raw =
        crate_declares_docker(repo_config.as_ref(), args.crate_name.as_deref());
    let docker_explicitly_requested =
        args.require_tools || explicit_stages.contains(&StageId::Docker);
    let (docker_configs, docker_declared) = if stages.contains(&StageId::Docker) {
        classify_docker_stage_state(
            resolve_docker_configs(
                repo_config.as_ref(),
                args.crate_name.as_deref(),
                child_snapshot,
                &log,
            ),
            docker_declared_raw,
            docker_explicitly_requested,
            &log,
        )?
    } else {
        (Vec::new(), docker_declared_raw)
    };

    // Resolve the config-only tool requirements for the gate, so a
    // host-default producer whose backing binary is absent hard-fails under
    // `--require-tools` instead of failing mid-run (msi) or silently
    // warn-skipping (upx). Each is resolved from config by the SAME helper the
    // build / release preflight consults, so the gate probe can never drift
    // from the binary the build spawns. Only resolved when the stage is
    // actually in the set; an empty resolution is not inserted (the stage then
    // carries no requirement).
    //
    // - `msi`: the WiX binaries the resolved version needs — a `version: v3`
    //   config needs candle+light, not the v4 `wix` CLI.
    // - `upx`: each enabled `upx:` entry's binary (default `upx`).
    let mut config_tools: BTreeMap<StageId, Vec<String>> = BTreeMap::new();
    if stages.contains(&StageId::Msi) {
        let tools = resolve_msi_tools(repo_config.as_ref());
        if !tools.is_empty() {
            config_tools.insert(StageId::Msi, tools);
        }
    }
    if stages.contains(&StageId::Upx) {
        let tools = resolve_upx_tools(repo_config.as_ref());
        if !tools.is_empty() {
            config_tools.insert(StageId::Upx, tools);
        }
    }

    let harness = Harness {
        repo_root: repo_root.clone(),
        commit: commit.clone(),
        stages,
        explicit_stages,
        require_tools: args.require_tools,
        runs: args.runs,
        sde,
        allowlist,
        report_path: report_path.clone(),
        inject_drift,
        targets,
        preserve_dist,
        version_hint,
        child_snapshot,
        docker_backend_hint,
        docker_configs,
        docker_declared,
        crate_name: args.crate_name.clone(),
        verbosity,
        config_tools,
        disk_abs_floor_bytes: anodizer_core::disk::abs_floor_bytes_from_env(),
        disk_safety_factor: anodizer_core::disk::safety_factor_from_env(),
    };

    let report = harness.run()?;

    if let Some(parent) = report_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating report directory {}", parent.display()))?;
    }
    let json =
        serde_json::to_string_pretty(&report).context("serializing determinism report to JSON")?;
    std::fs::write(&report_path, json)
        .with_context(|| format!("writing report to {}", report_path.display()))?;
    if verbosity > Verbosity::Quiet {
        eprintln!(
            "{}",
            render_note(&format!(
                "wrote determinism report to {}",
                report_path.display()
            ))
        );
    }

    if report.drift_count > 0 {
        eprintln!(
            "{}",
            render_error(&format!(
                "drift detected: {} artifact(s) differed across {} runs",
                report.drift_count, report.runs
            ))
        );
        for d in &report.drift {
            // Surface `differing_bytes_summary` alongside hashes. The
            // summary is already computed by the harness and stored in
            // `determinism.json`, but the JSON only ships if the publish
            // job runs — and publish is gated on determinism passing.
            // Printing here makes the offset hint (e.g. `first diff at
            // offset 0x130`) visible directly in CI logs (90-day
            // retention), surviving even when the run's artifacts expire.
            let detail = match &d.differing_bytes_summary {
                Some(summary) => format!("{}: {} | {:?}", d.artifact, summary, d.hashes),
                None => format!("{}: {:?}", d.artifact, d.hashes),
            };
            log.failure(&detail);
        }
        // Use the conventional process::exit so the gate is observable
        // from CI even if a caller wraps the binary in a script.
        std::process::exit(1);
    }

    // No drift: reaching here means every rebuild produced byte-identical
    // artifacts. Emit one concise default RESULT so a passing run is not
    // silent (the drift path above is the only loud branch otherwise).
    log.success(&format!(
        "{}/{} runs byte-identical",
        report.runs, report.runs
    ));

    Ok(())
}

mod resolve;
mod stages;

use resolve::*;
use stages::*;

#[cfg(test)]
mod tests;
