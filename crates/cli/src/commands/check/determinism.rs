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

use crate::determinism_harness::{Harness, StageId, installer_stages};
use anodizer_cli::CheckDeterminismArgs;
use anodizer_core::{
    AllowList, AllowListEntry, DeterminismState,
    git::{head_commit_hash_in, head_commit_timestamp_in, head_is_at_tag, resolve_snapshot_sde},
    log::{StageLogger, Verbosity, render_error, render_note},
};
use anyhow::{Context, Result};

pub fn run(args: CheckDeterminismArgs, verbose: bool, debug: bool, quiet: bool) -> Result<()> {
    let verbosity = Verbosity::from_flags(quiet, verbose, debug);
    let repo_root = std::env::current_dir().context("resolving repo root")?;

    // `--inject-drift` is a test-only flag gated by
    // `ANODIZE_TEST_HARNESS=1`. The flag is hidden from `--help`, so the
    // only way for an operator to trip the rejection branch is to type
    // it deliberately; the hard error keeps the surface from being
    // exercised accidentally on production releases.
    let inject_drift = if std::env::var("ANODIZE_TEST_HARNESS").as_deref() == Ok("1") {
        args.inject_drift.clone()
    } else if args.inject_drift.is_some() {
        anyhow::bail!("--inject-drift requires ANODIZE_TEST_HARNESS=1 (test-harness gated flag)");
    } else {
        None
    };

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

/// Parse a comma-separated stage subset (`--stages=build,archive,...`).
///
/// Returns `Err` on unknown tokens — silently dropping typos like
/// `--stages=archve,checksum` (note the missing `i`) is a UX trap that
/// quietly under-verifies the release; the operator typed a stage they
/// expected to be exercised. Empty / whitespace-only tokens (e.g. a
/// trailing comma) are tolerated. Both an absent flag and an empty
/// selection (`--stages=""`) fall back to [`default_stages_for_host`] —
/// the OS-native partition the harness builds when no filter is given.
fn parse_stages(s: Option<&str>) -> Result<Vec<StageId>, String> {
    // Umbrella selector for every installer-family stage. Operators
    // type `--stages=installers` to exercise the full set in one shot;
    // individual family stages (`msi`, `nsis`, ...) remain available
    // for narrower runs. Delegating to the harness's
    // `installer_detect::installer_stages` keeps the CLI parser and
    // harness gate consulting the same source of truth.
    match s {
        None => Ok(default_stages_for_host()),
        Some(list) => {
            let mut parsed: Vec<StageId> = Vec::new();
            let mut unknown: Vec<String> = Vec::new();
            for tok in list.split(',') {
                let tok = tok.trim();
                if tok.is_empty() {
                    // Tolerate trailing / empty tokens (e.g.
                    // `archive,checksum,`); the operator clearly meant
                    // the named stages and the empty slot is noise.
                    continue;
                }
                match tok {
                    "build" => parsed.push(StageId::Build),
                    "source" => parsed.push(StageId::Source),
                    "upx" => parsed.push(StageId::Upx),
                    "archive" => parsed.push(StageId::Archive),
                    "nfpm" => parsed.push(StageId::Nfpm),
                    "makeself" => parsed.push(StageId::Makeself),
                    "snapcraft" => parsed.push(StageId::Snapcraft),
                    "sbom" => parsed.push(StageId::Sbom),
                    "sign" => parsed.push(StageId::Sign),
                    "checksum" => parsed.push(StageId::Checksum),
                    "cargo-package" => parsed.push(StageId::CargoPackage),
                    "docker" => parsed.push(StageId::Docker),
                    "msi" => parsed.push(StageId::Msi),
                    "nsis" => parsed.push(StageId::Nsis),
                    "dmg" => parsed.push(StageId::Dmg),
                    "pkg" => parsed.push(StageId::Pkg),
                    "srpm" => parsed.push(StageId::Srpm),
                    "appbundle" => parsed.push(StageId::Appbundle),
                    "appimage" => parsed.push(StageId::Appimage),
                    "flatpak" => parsed.push(StageId::Flatpak),
                    "installers" => parsed.extend(installer_stages()),
                    other => unknown.push(other.to_string()),
                }
            }
            if !unknown.is_empty() {
                return Err(format!(
                    "--stages contained unknown stage(s): {}. \
                     Known stages: build, source, upx, archive, nfpm, makeself, snapcraft, sbom, sign, checksum, cargo-package, docker, msi, nsis, dmg, pkg, srpm, appbundle, appimage, flatpak, installers.",
                    unknown.join(", ")
                ));
            }
            // De-dup while preserving insertion order so
            // `--stages=installers,msi` (umbrella followed by an
            // individual member) doesn't list `msi` twice in
            // `stages_under_test`. The first mention wins, matching
            // the operator's typed intent.
            let mut seen: std::collections::HashSet<StageId> = std::collections::HashSet::new();
            let mut deduped: Vec<StageId> = Vec::with_capacity(parsed.len());
            for stage in parsed {
                if seen.insert(stage) {
                    deduped.push(stage);
                }
            }
            Ok(if deduped.is_empty() {
                default_stages_for_host()
            } else {
                deduped
            })
        }
    }
}

/// The OS-appropriate stage partition the harness builds when `--stages` is
/// absent — "no filter" means "byte-verify everything this host can natively
/// produce", never a minimal subset that silently under-covers a release.
///
/// This encodes the partition that USED to live as a hand-written
/// `det_stages:` key per shard in `.github/workflows/determinism.yml`; the
/// per-OS "what is appropriate to build here" decision is intrinsic to the
/// tool, not a CI concern, so it belongs in the harness. `--stages=` remains
/// a USER filter layered on top.
///
/// ## Why the partition is per-OS (payload-binary routing)
///
/// The determinism harness is sharded by host precisely because one host
/// cannot cross-compile every target's binary, and a produce-stage emits
/// nothing on a shard that lacks its payload binary — so each installer must
/// run on the shard that natively builds what it packages:
///
/// - `appbundle` / `dmg` / `pkg` → **macOS** (need the darwin binary). On
///   macOS `appbundle` precedes `dmg`/`pkg` so their `use: appbundle` finds a
///   source `.app`.
/// - `msi` / `nsis` → **Windows** (need the windows-msvc binary).
/// - `docker` / `appimage` / `flatpak` / `nfpm` / `makeself` / `snapcraft` /
///   `srpm` → **Linux**.
///
/// Routing an installer to a shard without its payload binary is how these
/// formats silently shipped in NO release for so long (they were listed only
/// on the linux-only ubuntu shard, which produces no darwin/windows binary).
///
/// ## Per-format reproducibility verdict
///
/// The harness byte-compares the GATED formats and counts any drift as a
/// regression; the ALLOWLISTED ones are intrinsically non-reproducible (see
/// `anodizer_core::DeterminismState::seed_from_commit`) and excluded from
/// `drift_count` while still surfaced in the report:
///
/// - `appbundle` — **GATED**: pure file assembly, byte-reproducible
///   (`appbundle_is_byte_reproducible_across_time`).
/// - `nsis` — **GATED**: `makensis` honors `SOURCE_DATE_EPOCH`, byte-
///   reproducible (`nsis_setup_is_byte_reproducible_across_time`).
/// - `dmg` — **ALLOWLISTED**: `hdiutil` writes a fresh UDIF koly SegmentID
///   GUID per run; native, non-reproducible.
/// - `pkg` — **ALLOWLISTED**: macOS-native `pkgbuild` stamps a wall-clock xar
///   TOC and ignores `SOURCE_DATE_EPOCH`.
/// - `msi` — **ALLOWLISTED**: WiX regenerates a random PackageCode GUID plus
///   Created/LastModified (wixtoolset/issues#8978).
///
/// ## Tool gate
///
/// The tool gate (see [`crate::determinism_harness`]'s
/// `gate_installer_stages` / the docker fork) further prunes any stage in
/// this default whose backing tool is absent on the host. By default a
/// host-default stage warn-skips so the harness stays usable everywhere (only
/// an explicitly typed `--stages=<stage>` hard-fails). Under CI's
/// `--require-tools` the WHOLE resolved set is promoted to hard-fail, so a
/// missing OS-native producer tool fails the shard rather than silently
/// under-covering the release.
///
/// `cargo-package` is intentionally NOT in this default: it is a harness-only
/// cross-platform probe of `cargo package` byte-stability, not a shipped
/// artifact, so it stays opt-in via `--stages=cargo-package`.
///
/// This returns the config-INDEPENDENT OS partition; the resolved default
/// applied when `--stages` is absent is this set intersected with the
/// config-configured producers — see [`host_default_for_config`].
fn default_stages_for_host() -> Vec<StageId> {
    let mut stages = ALWAYS_ON_STAGES.to_vec();
    if cfg!(target_os = "linux") {
        stages.extend([
            StageId::Nfpm,
            StageId::Makeself,
            StageId::Snapcraft,
            StageId::Srpm,
            StageId::Docker,
            StageId::Appimage,
            StageId::Flatpak,
        ]);
    } else if cfg!(target_os = "macos") {
        stages.extend([StageId::Appbundle, StageId::Dmg, StageId::Pkg]);
    } else if cfg!(target_os = "windows") {
        stages.extend([StageId::Msi, StageId::Nsis]);
    }
    stages
}

/// Stages produced for ANY config — they carry no installer tool and emit
/// nothing when unconfigured, so the host default keeps them unconditionally
/// rather than gating on config. Everything else in
/// [`default_stages_for_host`] is a config-gated producer pruned by
/// [`host_default_for_config`].
const ALWAYS_ON_STAGES: &[StageId] = &[
    StageId::Build,
    StageId::Source,
    StageId::Upx,
    StageId::Archive,
    StageId::Sbom,
    StageId::Sign,
    StageId::Checksum,
];

/// The resolved DEFAULT stage set (the `--stages`-absent path): the OS-native
/// partition ([`default_stages_for_host`]) with each config-gated producer
/// kept only when the loaded config actually configures it.
///
/// Determinism can only byte-verify artifacts the config PRODUCES, so a
/// generic consumer whose `.anodizer.yaml` has no `flatpaks:` block must not
/// get `flatpak` in its default — otherwise `--require-tools` would hard-fail
/// on a missing `flatpak-builder` for an artifact that project never builds.
/// The configured-producer set is the core SSOT
/// [`anodizer_core::env_preflight::configured_producer_stages`] (the same
/// `Config`/`CrateConfig` fields the pipeline's stage gates read); the
/// always-on base ([`ALWAYS_ON_STAGES`]) is never gated.
///
/// `config` must already have `apply_defaults` run on it (producers declared
/// under `defaults:` materialize onto crates). `None` (config failed to load)
/// falls back to the full OS partition — the conservative "do not silently
/// under-verify" choice; a genuine config-load failure surfaces from the
/// pipeline itself.
///
/// Only the DEFAULT path is intersected: an EXPLICIT `--stages=<x>` is the
/// operator's typed intent and is left exactly as parsed (it still hard-fails
/// on a missing tool, config notwithstanding).
fn host_default_for_config(config: Option<&anodizer_core::config::Config>) -> Vec<StageId> {
    let full = default_stages_for_host();
    let Some(config) = config else {
        return full;
    };
    let configured = anodizer_core::env_preflight::configured_producer_stages(config);
    full.into_iter()
        .filter(|s| ALWAYS_ON_STAGES.contains(s) || configured.contains(s.as_str()))
        .collect()
}

/// Whether `--stages` carries an EXPLICIT operator selection — at least one
/// non-blank token. `None`, `Some("")`, and `Some(",, ")` are all non-explicit
/// (they resolve to the host default). The single predicate behind both the
/// stage-set resolution ([`resolve_stages`]) and the explicit-stages hard-fail
/// set, so the two cannot disagree about what counts as "operator typed it".
fn is_explicit_stage_selection(stages_arg: Option<&str>) -> bool {
    matches!(stages_arg, Some(list) if list.split(',').any(|t| !t.trim().is_empty()))
}

/// Resolve the stage set under test from the `--stages` argument and the
/// loaded (defaults-applied) config.
///
/// An EXPLICIT selection (≥1 real token) is the operator's typed intent and
/// passes straight through [`parse_stages`], unchanged by config. An absent or
/// all-empty `--stages` resolves to the config-intersected host default
/// ([`host_default_for_config`]).
fn resolve_stages(
    stages_arg: Option<&str>,
    config: Option<&anodizer_core::config::Config>,
) -> Result<Vec<StageId>, String> {
    if is_explicit_stage_selection(stages_arg) {
        parse_stages(stages_arg)
    } else {
        Ok(host_default_for_config(config))
    }
}

/// Parse a comma-separated triple list (`--targets=x86_64-...,aarch64-...`).
///
/// Thin wrapper over `commands::helpers::parse_csv_list` that supplies
/// the `--targets`-shaped error hint. Unlike `--stages=<csv>`, there is
/// no closed vocabulary to validate against here — the legal set is
/// whatever appears in the project's `.anodizer.yaml` `targets` list,
/// and that's resolved later in the pipeline.
fn parse_targets(s: Option<&str>) -> Result<Option<Vec<String>>, String> {
    crate::commands::helpers::parse_csv_list(
        s,
        "--targets=x86_64-unknown-linux-gnu,aarch64-unknown-linux-gnu",
    )
}

/// Truncate a commit hash to the conventional 7-char "short" form, used
/// in the default `dist/run-<short>/determinism.json` path.
fn commit_short(commit: &str) -> String {
    commit.get(..7).unwrap_or(commit).to_string()
}

/// Emit the run-configuration summary beneath the `Checking determinism`
/// header as aligned `kv` detail rows (targets / stages / runs, plus
/// preserve-dist / crate when set). `targets` is `None` when the operator
/// did not pass `--targets` (the harness resolves the project's full target
/// list), rendered as `all (from config)` so the row is never blank.
///
/// This is the only printer of these parameters: callers (including the
/// `anodizer-action` wrapper) must not echo their own copy of the header
/// or the parameter rows.
fn emit_run_summary(
    log: &StageLogger,
    targets: Option<&[String]>,
    stages: &[StageId],
    runs: u32,
    preserve_dist: Option<&std::path::Path>,
    crate_name: Option<&str>,
) {
    let targets_value = match targets {
        Some(t) if !t.is_empty() => t.join(", "),
        _ => "all (from config)".to_string(),
    };
    let stages_value = stages
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let preserve_value = preserve_dist.map(|p| p.display().to_string());

    let mut rows: Vec<(&str, &str)> = vec![("targets", targets_value.as_str())];
    rows.push(("stages", stages_value.as_str()));
    let runs_value = runs.to_string();
    rows.push(("runs", runs_value.as_str()));
    if let Some(ref v) = preserve_value {
        rows.push(("preserve-dist", v.as_str()));
    }
    if let Some(name) = crate_name {
        rows.push(("crate", name));
    }
    // Pad every key to the widest EMITTED key so the value column lines up
    // across rows without reserving width for absent optional rows.
    let key_width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, value) in rows {
        log.kv(key, value, key_width);
    }
}

/// Resolve the harness's `child_snapshot` flag.
///
/// ```text
/// snapshot | no_snapshot | head_at_tag | child_snapshot | reason
/// ---------+-------------+-------------+----------------+--------
///  true    | -           | -           | true           | explicit --snapshot
///  -       | true        | -           | false          | explicit --no-snapshot
///  false   | false       | true        | false          | auto: tagged → release artifacts
///  false   | false       | false       | true           | auto: untagged → snapshot artifacts
/// ```
///
/// Free function so the matrix is unit-testable without forking git.
fn resolve_child_snapshot(snapshot: bool, no_snapshot: bool, head_at_tag: bool) -> bool {
    if snapshot {
        true
    } else if no_snapshot {
        false
    } else {
        !head_at_tag
    }
}

/// Extract the literal filename suffix a `signature:` template appends
/// after the artifact reference — the text following the final `}}`
/// template expansion (e.g. `{{ .Artifact }}.cosign.bundle` →
/// `.cosign.bundle`, `{{ .Artifact }}.sig` → `.sig`).
///
/// Returns `None` when there is no usable dotted extension to anchor a
/// `*.<ext>` allow-list pattern on (empty tail, a bare `.`, or a template
/// that signs in place without adding an extension). The guard is
/// load-bearing: a tail of `""` would yield a bare `*` (allow-listing every
/// artifact) and a tail of `"."` would yield `*.` (matching any name ending
/// in a dot) — both would silently suppress real drift. Require at least
/// one extension character after the leading dot.
///
/// This also (correctly) returns `None` when the final path segment is
/// itself an expansion — e.g. `{{ .Artifact }}.{{ .Format }}` or
/// `sigs/{{ .ArtifactName }}`. There the text after the last `}}` is empty
/// (or has no leading-dot literal), so no static suffix exists to anchor an
/// allow-list pattern on. Such templates can't be reduced to a `*.<ext>`
/// glob; the harness falls back to its other classification paths rather
/// than minting a meaningless or over-broad entry.
fn signature_suffix(template: &str) -> Option<String> {
    let tail = match template.rfind("}}") {
        Some(idx) => &template[idx + 2..],
        None => template,
    };
    let tail = tail.trim();
    if tail.len() < 2 || !tail.starts_with('.') {
        return None;
    }
    Some(tail.to_string())
}

/// Derive allow-list entries for signature artifacts from the project's
/// `signs:` / `binary_signs:` signature templates (top-level and per
/// workspace).
///
/// Signatures are non-reproducible by nature: cosign signs with a random
/// ECDSA nonce, so its bundle/signature bytes differ on every signing of
/// byte-identical input. `infer_stage_from_path` already classifies the
/// default `.sig` / `.pem` / `.cert` suffixes as the `sign` stage (which
/// the harness auto-allow-lists), but the `signature:` template is
/// user-configurable, so a custom suffix (cfgd's `.cosign.bundle`) would
/// fall through to `unknown` and be counted as drift. Deriving the
/// suffixes from config keeps the harness correct for any naming scheme.
///
/// Pure: collect the
/// distinct signature suffixes configured across top-level and per-
/// workspace `signs:` / `binary_signs:`, and map each to a `*<suffix>`
/// allow-list entry. Factored out so the suffix logic is unit-testable
/// without the cwd-dependent config load.
fn signature_allowlist_entries_from_config(
    cfg: &anodizer_core::config::Config,
) -> Vec<AllowListEntry> {
    use anodizer_core::config::SignConfig;

    let mut suffixes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut collect = |entries: &[SignConfig], default_tmpl: &str| {
        for s in entries {
            if let Some(suffix) = signature_suffix(s.resolved_signature_template(default_tmpl)) {
                suffixes.insert(suffix);
            }
        }
    };
    collect(&cfg.signs, SignConfig::DEFAULT_SIGNATURE_TEMPLATE);
    collect(
        &cfg.binary_signs,
        SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE,
    );
    for w in cfg.workspaces.iter().flatten() {
        collect(&w.signs, SignConfig::DEFAULT_SIGNATURE_TEMPLATE);
        collect(
            &w.binary_signs,
            SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE,
        );
    }

    suffixes
        .into_iter()
        .map(|suffix| AllowListEntry {
            reason: format!(
                "signature artifact ({suffix}): signature bytes vary by signer \
                 (cosign signs with a random ECDSA nonce); validate cryptographically \
                 via `cosign verify-blob` / `gpg --verify`, not byte-equality"
            ),
            artifact: format!("*{suffix}"),
        })
        .collect()
}

/// Probe the project's `dockers_v2[*].use` field for a `"podman"` opt-in.
///
/// Returns `Some("podman")` when any `dockers_v2` entry under any crate
/// (or the project-level `defaults.dockers_v2`) sets `use: podman`,
/// `Some("buildx")` when only buildx is configured, and `None` when no
/// `dockers_v2` entries exist. The harness consults the hint to decide
/// whether to short-circuit its `docker buildx`-based reproducibility
/// probe.
fn detect_docker_backend_hint(cfg: &anodizer_core::config::Config) -> Option<String> {
    let mut saw_buildx = false;
    let mut iter: Vec<&Option<String>> = Vec::new();
    if let Some(ref defaults) = cfg.defaults
        && let Some(ref v2) = defaults.dockers_v2
    {
        iter.push(&v2.use_backend);
    }
    for c in &cfg.crates {
        if let Some(ref v2s) = c.dockers_v2 {
            for v in v2s {
                iter.push(&v.use_backend);
            }
        }
    }
    for opt in iter {
        match opt.as_deref() {
            Some("podman") => return Some("podman".to_string()),
            Some("buildx") | None => saw_buildx = true,
            Some(_) => {}
        }
    }
    if saw_buildx {
        Some("buildx".to_string())
    } else {
        None
    }
}

/// Resolve the WiX binaries the `msi` stage requires from the loaded config
/// via [`anodizer_stage_msi::required_msi_tools`] — the SAME helper
/// env-preflight consults, so the determinism gate's MSI tool requirement
/// can never drift from the version the build runs (WiX v3 → candle+light,
/// v4 → wix, the Linux path → wixl). Resolution covers all config modes
/// (single / lockstep / per-crate) because `required_msi_tools` iterates the
/// full `crate_universe` and resolves each crate's `msis:` entry under the
/// project Context.
///
/// A missing/unparseable config (`None`) yields an empty list: the gate then
/// treats `msi` as carrying no tool requirement and the real config error
/// surfaces from the pipeline itself.
///
/// `required_msi_tools` renders each entry's `skip:` / `if:` in this bare
/// gate context, which lacks the `--snapshot` child's `.Version` /
/// `IsSnapshot` / `.Env` vars — so a context-dependent skip/if could resolve
/// an entry inactive here yet active in the child, leaving `msi` ungated.
/// Unlike `upx`, that is benign: when no WiX binary is on PATH the stage's
/// version probe falls back to v4 and the child hard-fails at `wix build`
/// spawn (`run_checked`), surfacing the missing tool loudly. There is no
/// silent warn-skip to under-cover, so `msi` needs no conservative
/// over-require (contrast [`resolve_upx_tools`], whose stage warn-skips).
fn resolve_msi_tools(repo_config: Option<&anodizer_core::config::Config>) -> Vec<String> {
    let Some(cfg) = repo_config else {
        return Vec::new();
    };
    let ctx = anodizer_core::context::Context::new(
        cfg.clone(),
        anodizer_core::context::ContextOptions::default(),
    );
    anodizer_stage_msi::required_msi_tools(&ctx)
}

/// Resolve the upx binaries the `upx` stage requires from the loaded config
/// via [`anodizer_stage_upx::required_upx_tools`] — the SAME helper release
/// preflight consults, so the determinism gate's upx requirement can never
/// drift from what the build runs. Each enabled `upx:` entry contributes its
/// `binary` (default `upx`).
///
/// A missing/unparseable config (`None`) yields an empty list: the gate then
/// treats `upx` as carrying no tool requirement and the stage's own runtime
/// guard governs.
///
/// ## Conservative over-require for a templated `enabled:`
///
/// `required_upx_tools` renders each `enabled:` in this bare gate context,
/// which lacks the `--snapshot` child's `.Version` / `IsSnapshot` /
/// `IsHarness` / `.Env` template vars. A context-DEPENDENT `enabled:` can
/// therefore render `false` here yet `true` in the child — and the upx stage
/// WARN-SKIPS a missing binary at default strictness (`UpxStage::run` →
/// `Context::strict_guard`, which only bails under `options.strict`; the
/// determinism child release is not strict). That under-resolution is exactly
/// the silent false coverage `--require-tools` exists to forbid. So any entry
/// whose `enabled:` is a template forces its binary into the requirement set:
/// the gate must never UNDER-require. A literal `enabled: true` / `false` is
/// context-free and stays precisely resolved by the SSOT.
fn resolve_upx_tools(repo_config: Option<&anodizer_core::config::Config>) -> Vec<String> {
    let Some(cfg) = repo_config else {
        return Vec::new();
    };
    let ctx = anodizer_core::context::Context::new(
        cfg.clone(),
        anodizer_core::context::ContextOptions::default(),
    );
    let mut tools = anodizer_stage_upx::required_upx_tools(&ctx);
    for entry in &cfg.upx {
        if entry.enabled.as_ref().is_some_and(|e| e.is_template()) && !tools.contains(&entry.binary)
        {
            tools.push(entry.binary.clone());
        }
    }
    tools
}

/// Read the target project's release version from `<repo>/Cargo.toml`.
///
/// Resolves `[workspace.package].version` first (workspace inheritance,
/// as cfgd uses to share one version across crates), then falls back to
/// `[package].version`. Returns `None` if the manifest is missing,
/// unparseable, or has neither key.
fn read_project_version(repo_root: &std::path::Path) -> Option<String> {
    let manifest = repo_root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).ok()?;
    let doc: toml::Value = toml::from_str(&text).ok()?;
    doc.get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            doc.get("package")
                .and_then(|p| p.get("version"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_msi_tools_threads_resolved_wix_tool_from_config() {
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};

        // The dispatcher must thread the WiX tool requirement resolved from
        // config into the harness gate — not a host-static guess. A `version:
        // v4` config resolves deterministically to the single `wix` CLI
        // (V4 is never downgraded), so this proves the config→tools wiring
        // independent of which WiX binaries the test host happens to carry.
        // (The host-aware v3 → candle+light path — the actual release-blocker
        // — is pinned by `anodizer_stage_msi`'s `required_msi_tools` tests.)
        let msi_cfg = MsiConfig {
            wxs: Some("app.wxs".to_string()),
            version: Some("v4".to_string()),
            ..Default::default()
        };
        let config = Config {
            project_name: "myapp".to_string(),
            crates: vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                msis: Some(vec![msi_cfg]),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(resolve_msi_tools(Some(&config)), vec!["wix".to_string()]);
    }

    #[test]
    fn resolve_msi_tools_none_config_is_empty() {
        assert!(resolve_msi_tools(None).is_empty());
    }

    #[test]
    fn resolve_upx_tools_threads_enabled_binary_from_config() {
        use anodizer_core::config::{Config, StringOrBool, UpxConfig};

        // The dispatcher must thread each enabled `upx:` entry's binary into
        // the harness gate so `--require-tools` can hard-fail a host-default
        // upx run whose binary is absent. A disabled entry contributes nothing.
        let config = Config {
            project_name: "myapp".to_string(),
            upx: vec![
                UpxConfig {
                    enabled: Some(StringOrBool::Bool(true)),
                    binary: "upx".to_string(),
                    ..Default::default()
                },
                UpxConfig {
                    enabled: Some(StringOrBool::Bool(false)),
                    binary: "other-upx".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(resolve_upx_tools(Some(&config)), vec!["upx".to_string()]);
    }

    #[test]
    fn resolve_upx_tools_none_config_is_empty() {
        assert!(resolve_upx_tools(None).is_empty());
    }

    #[test]
    fn resolve_upx_tools_force_requires_templated_enabled() {
        use anodizer_core::config::{Config, StringOrBool, UpxConfig};

        // A context-dependent `enabled:` can render false in the bare gate
        // context yet true in the `--snapshot` child. Because the upx stage
        // WARN-SKIPS a missing binary (silent false coverage), the gate must
        // over-require: a templated `enabled` forces its binary in even when
        // the bare-context render is false. This template renders literally
        // `false` here (no vars), so `required_upx_tools` alone would drop it —
        // proving the conservative pass, not the SSOT, is what adds it.
        let config = Config {
            project_name: "myapp".to_string(),
            upx: vec![UpxConfig {
                enabled: Some(StringOrBool::String(
                    "{{ if false }}true{{ else }}false{{ end }}".to_string(),
                )),
                binary: "upx".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(resolve_upx_tools(Some(&config)), vec!["upx".to_string()]);
    }

    #[test]
    fn resolve_upx_tools_omits_literal_false_enabled() {
        use anodizer_core::config::{Config, StringOrBool, UpxConfig};

        // The conservative pass must fire ONLY for templates: a literal
        // `enabled: false` is context-free, so the gate trusts the SSOT and
        // carries no requirement (no spurious hard-fail under --require-tools).
        let config = Config {
            project_name: "myapp".to_string(),
            upx: vec![UpxConfig {
                enabled: Some(StringOrBool::Bool(false)),
                binary: "upx".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(resolve_upx_tools(Some(&config)).is_empty());
    }

    #[test]
    fn parse_stages_default_returns_host_native_partition() {
        // No `--stages` resolves to the OS-native partition (the encoded
        // `det_stages` that used to live per-shard in determinism.yml), not a
        // minimal subset that would silently under-cover the release.
        let stages = parse_stages(None).expect("None is always Ok");
        assert_eq!(stages, default_stages_for_host());
        // The common base is present on every OS.
        for base in [
            StageId::Build,
            StageId::Source,
            StageId::Upx,
            StageId::Archive,
            StageId::Sbom,
            StageId::Sign,
            StageId::Checksum,
        ] {
            assert!(stages.contains(&base), "base stage {base:?} missing");
        }
    }

    #[test]
    fn default_stages_for_host_includes_os_native_producers() {
        let stages = default_stages_for_host();
        // cargo-package is harness-only and stays opt-in on every OS.
        assert!(
            !stages.contains(&StageId::CargoPackage),
            "cargo-package must never be in the host default"
        );
        #[cfg(target_os = "linux")]
        for s in [
            StageId::Nfpm,
            StageId::Makeself,
            StageId::Snapcraft,
            StageId::Srpm,
            StageId::Docker,
            StageId::Appimage,
            StageId::Flatpak,
        ] {
            assert!(stages.contains(&s), "linux default missing {s:?}");
        }
        #[cfg(target_os = "macos")]
        for s in [StageId::Appbundle, StageId::Dmg, StageId::Pkg] {
            assert!(stages.contains(&s), "macos default missing {s:?}");
        }
        #[cfg(target_os = "windows")]
        for s in [StageId::Msi, StageId::Nsis] {
            assert!(stages.contains(&s), "windows default missing {s:?}");
        }
    }

    #[test]
    fn parse_stages_accepts_appimage_and_flatpak() {
        let stages = parse_stages(Some("appimage,flatpak")).expect("both are known stages");
        assert_eq!(stages, vec![StageId::Appimage, StageId::Flatpak]);
    }

    /// A minimal config (one crate, no producer blocks) must resolve the
    /// `--stages`-absent default to the always-on base ONLY — every config-
    /// gated producer is pruned, so `--require-tools` cannot hard-fail on a
    /// tool for an artifact this project never builds.
    #[test]
    fn host_default_excludes_unconfigured_producers() {
        use anodizer_core::config::{Config, CrateConfig};
        let config = Config {
            project_name: "minimal".to_string(),
            crates: vec![CrateConfig {
                name: "minimal".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let stages = host_default_for_config(Some(&config));
        // Base stays unconditionally.
        for base in ALWAYS_ON_STAGES {
            assert!(stages.contains(base), "base stage {base:?} must remain");
        }
        // No config-gated producer survives on any OS.
        for gated in [
            StageId::Nfpm,
            StageId::Makeself,
            StageId::Snapcraft,
            StageId::Srpm,
            StageId::Docker,
            StageId::Appimage,
            StageId::Flatpak,
            StageId::Appbundle,
            StageId::Dmg,
            StageId::Pkg,
            StageId::Msi,
            StageId::Nsis,
        ] {
            assert!(
                !stages.contains(&gated),
                "unconfigured producer {gated:?} must be pruned from the default"
            );
        }
    }

    /// A config that DOES configure the Linux producers must keep them in the
    /// resolved default (so they are byte-verified, and `--require-tools`
    /// legitimately requires their tools). Mixes per-crate blocks (nfpm /
    /// snapcraft / flatpak / docker) and top-level blocks (appimage / makeself
    /// / srpm) to cover both detection paths.
    #[cfg(target_os = "linux")]
    #[test]
    fn host_default_includes_configured_linux_producers() {
        use anodizer_core::config::{
            AppImageConfig, Config, CrateConfig, DockerV2Config, FlatpakConfig, MakeselfConfig,
            NfpmConfig, SnapcraftConfig, SrpmConfig,
        };
        let config = Config {
            project_name: "full".to_string(),
            crates: vec![CrateConfig {
                name: "full".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                nfpms: Some(vec![NfpmConfig::default()]),
                snapcrafts: Some(vec![SnapcraftConfig::default()]),
                flatpaks: Some(vec![FlatpakConfig::default()]),
                dockers_v2: Some(vec![DockerV2Config::default()]),
                ..Default::default()
            }],
            appimages: vec![AppImageConfig::default()],
            makeselfs: vec![MakeselfConfig::default()],
            srpms: Some(SrpmConfig::default()),
            ..Default::default()
        };
        let stages = host_default_for_config(Some(&config));
        for producer in [
            StageId::Nfpm,
            StageId::Makeself,
            StageId::Snapcraft,
            StageId::Srpm,
            StageId::Docker,
            StageId::Appimage,
            StageId::Flatpak,
        ] {
            assert!(
                stages.contains(&producer),
                "configured producer {producer:?} must stay in the default"
            );
        }
    }

    /// `None` config (load failed) falls back to the full OS partition — the
    /// conservative "do not silently under-verify" choice.
    #[test]
    fn host_default_none_config_is_full_partition() {
        assert_eq!(host_default_for_config(None), default_stages_for_host());
    }

    /// An EXPLICIT `--stages` is the operator's typed intent and ignores the
    /// config intersection entirely — `--stages=nfpm` resolves to `[nfpm]`
    /// even when the config configures no nfpm.
    #[test]
    fn resolve_stages_explicit_ignores_config() {
        use anodizer_core::config::{Config, CrateConfig};
        let bare = Config {
            crates: vec![CrateConfig {
                name: "x".to_string(),
                path: ".".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let stages = resolve_stages(Some("nfpm"), Some(&bare)).expect("nfpm is a known stage");
        assert_eq!(stages, vec![StageId::Nfpm]);
    }

    #[test]
    fn is_explicit_stage_selection_matches_nonblank_token_only() {
        // The single predicate behind both the stage-set resolution and the
        // explicit-stages hard-fail set: a real token is explicit; absent or
        // all-blank is the host default. Drift between the two call sites would
        // let a stage hard-fail in one path and warn-skip in the other.
        assert!(is_explicit_stage_selection(Some("msi")));
        assert!(is_explicit_stage_selection(Some(" archive , checksum ")));
        assert!(!is_explicit_stage_selection(None));
        assert!(!is_explicit_stage_selection(Some("")));
        assert!(!is_explicit_stage_selection(Some(" , , ")));
    }

    #[test]
    fn parse_stages_subset_filters_to_named_set() {
        let stages = parse_stages(Some("archive,checksum")).expect("all known stages");
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["archive", "checksum"]
        );
    }

    #[test]
    fn parse_stages_accepts_full_byte_stable_set() {
        // Every stage name reachable from anodizer-action's per-OS
        // determinism-stages default must parse cleanly. Drift between
        // this parser and the action's expanded default surfaces as
        // "unknown stage(s): makeself, snapcraft" in CI. This test pins
        // the parser to the action's current Linux default CSV.
        let stages = parse_stages(Some(
            "build,source,upx,archive,nfpm,makeself,snapcraft,sbom,sign,checksum",
        ))
        .expect("all stages in the action's Linux default must parse");
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec![
                "build",
                "source",
                "upx",
                "archive",
                "nfpm",
                "makeself",
                "snapcraft",
                "sbom",
                "sign",
                "checksum"
            ]
        );
    }

    #[test]
    fn parse_stages_errors_on_unknown_token() {
        // Typos like `--stages=archve,checksum` previously filtered to
        // just `checksum` and quietly under-verified. The unknown token
        // must surface as an error naming the bad token and the legal
        // vocabulary.
        let err = parse_stages(Some(" archive , bogus, checksum "))
            .expect_err("unknown token must error");
        assert!(
            err.contains("bogus") && err.contains("Known stages"),
            "error must name the bad token and the legal vocabulary: {err}"
        );
        // Multiple unknowns are reported together rather than failing on
        // the first — the operator gets a complete picture in one shot.
        let err = parse_stages(Some("archve,nope")).expect_err("multiple unknowns must error");
        assert!(
            err.contains("archve") && err.contains("nope"),
            "all unknown tokens must be named: {err}"
        );
    }

    #[test]
    fn parse_stages_tolerates_trailing_comma_and_whitespace() {
        // Empty / whitespace-only tokens (trailing comma, double comma,
        // surrounding spaces) are noise rather than typos.
        let stages = parse_stages(Some("archive,checksum,")).expect("trailing comma tolerated");
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["archive", "checksum"]
        );
        let stages = parse_stages(Some(" archive , , checksum ")).expect("empty middle tolerated");
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["archive", "checksum"]
        );
    }

    #[test]
    fn parse_stages_installers_umbrella_expands_to_full_set() {
        // `--stages=installers` is the operator-facing shorthand for
        // every installer-family stage. The expansion must include
        // nfpm + makeself + srpm + msi + nsis + dmg + pkg in the same
        // order `installer_stages()` advertises so the harness gate
        // and the parser stay aligned.
        let stages = parse_stages(Some("installers")).expect("umbrella token must parse");
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["nfpm", "makeself", "srpm", "msi", "nsis", "dmg", "pkg"]
        );
    }

    #[test]
    fn parse_stages_installers_dedupes_against_individual_members() {
        // `--stages=installers,msi` must not double-list `msi` in the
        // report's `stages_under_test`. First mention wins so the
        // operator's typed order is preserved.
        let stages =
            parse_stages(Some("installers,msi")).expect("umbrella + individual must parse");
        let names: Vec<&str> = stages.iter().map(|s| s.as_str()).collect();
        assert_eq!(names.iter().filter(|n| **n == "msi").count(), 1);
    }

    #[test]
    fn parse_stages_accepts_each_individual_installer_token() {
        // Every individual installer stage token must parse in
        // isolation so operators can narrow the harness to a single
        // family (`--stages=msi`) without invoking the umbrella.
        for token in ["msi", "nsis", "dmg", "pkg", "srpm"] {
            let stages = parse_stages(Some(token))
                .unwrap_or_else(|e| panic!("token `{token}` must parse: {e}"));
            assert_eq!(
                stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                vec![token]
            );
        }
    }

    #[test]
    fn parse_stages_accepts_appbundle_token() {
        // `appbundle` is pure file assembly (no tool) but must be a
        // first-class stage token: a `dmg`/`pkg` stage with `use:
        // appbundle` finds no source artifact unless `appbundle` is kept
        // out of the harness's child `--skip=` complement, which requires
        // it to be requestable here.
        let stages = parse_stages(Some("appbundle,dmg")).expect("appbundle token must parse");
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["appbundle", "dmg"]
        );
    }

    #[test]
    fn parse_stages_empty_string_falls_back_to_default() {
        // An empty / all-whitespace selection picks the OS-native host
        // partition so `--stages=""` doesn't degrade into a no-op.
        let expected = default_stages_for_host();
        let stages = parse_stages(Some("")).expect("empty list returns default");
        assert_eq!(stages, expected);
        let stages = parse_stages(Some(" , , ")).expect("whitespace-only returns default");
        assert_eq!(stages, expected);
    }

    #[test]
    fn parse_targets_default_is_none() {
        assert_eq!(parse_targets(None).unwrap(), None);
    }

    #[test]
    fn parse_targets_subset_filters_to_named_list() {
        let got = parse_targets(Some("x86_64-unknown-linux-gnu,aarch64-unknown-linux-gnu"))
            .expect("ascii triples accepted");
        assert_eq!(
            got,
            Some(vec![
                "x86_64-unknown-linux-gnu".to_string(),
                "aarch64-unknown-linux-gnu".to_string(),
            ])
        );
    }

    #[test]
    fn parse_targets_tolerates_trailing_comma_and_whitespace() {
        let got = parse_targets(Some(" x86_64-apple-darwin , aarch64-apple-darwin , "))
            .expect("trailing comma + spaces tolerated");
        assert_eq!(
            got,
            Some(vec![
                "x86_64-apple-darwin".to_string(),
                "aarch64-apple-darwin".to_string(),
            ])
        );
    }

    #[test]
    fn parse_targets_errors_on_all_empty_csv() {
        // Operator typed `--targets=""` or `--targets=", , "` — they
        // clearly meant to pass *something* but gave nothing. Silent
        // fallback to "no filter" would mask the typo and cross-compile
        // every configured target (the very bug Option B exists to
        // prevent).
        let err = parse_targets(Some("")).expect_err("empty CSV must error");
        assert!(
            err.contains("at least one entry"),
            "error must explain the requirement: {err}"
        );
        let err = parse_targets(Some(" , , ")).expect_err("whitespace-only CSV must error");
        assert!(
            err.contains("at least one entry"),
            "error must explain the requirement: {err}"
        );
    }

    #[test]
    fn commit_short_truncates_to_seven_chars() {
        assert_eq!(commit_short("abcdef1234567890"), "abcdef1");
    }

    #[test]
    fn commit_short_keeps_short_commit_as_is() {
        assert_eq!(commit_short("abc"), "abc");
    }

    /// The harness body is exercised by the integration test at
    /// `crates/cli/tests/check_determinism.rs`. Argument-plumbing
    /// behavior is covered by the unit tests above.
    #[test]
    fn dispatcher_args_are_consumed() {
        // Sanity guard: if the CheckDeterminismArgs surface grows new
        // required fields, this test fails to compile and forces the
        // dispatcher above to pick up the new field explicitly.
        let _args = CheckDeterminismArgs {
            runs: 2,
            stages: None,
            targets: None,
            report: None,
            snapshot: false,
            no_snapshot: false,
            inject_drift: None,
            preserve_dist: None,
            crate_name: None,
            require_tools: false,
        };
    }

    // ── resolve_child_snapshot ────────────────────────────────────────────

    #[test]
    fn resolve_child_snapshot_auto_off_when_head_at_tag() {
        // Tagged HEAD = cutting a release → produce-stages emit
        // release-named artifacts (no `-SNAPSHOT-<sha>` suffix). The
        // workflow's preserved-dist payload must be immediately
        // shippable via `--publish-only`.
        assert!(!resolve_child_snapshot(false, false, true));
    }

    #[test]
    fn resolve_child_snapshot_auto_on_when_head_not_at_tag() {
        // Untagged HEAD = local rehearsal → produce-stages emit
        // `-SNAPSHOT-<sha>`-suffixed artifacts so the bytes can't be
        // mistaken for a release build.
        assert!(resolve_child_snapshot(false, false, false));
    }

    #[test]
    fn resolve_child_snapshot_explicit_snapshot_beats_auto() {
        // `--snapshot` on a tagged HEAD: operator deliberately wants
        // snapshot-style artifacts even though HEAD is tagged. Auto
        // would say off; explicit must beat auto.
        assert!(resolve_child_snapshot(true, false, true));
        assert!(resolve_child_snapshot(true, false, false));
    }

    #[test]
    fn resolve_child_snapshot_explicit_no_snapshot_beats_auto() {
        // `--no-snapshot` on an untagged HEAD: legacy workflow override
        // — operator forces release-style artifact names even though
        // we're not at a tag. Auto would say on; explicit must beat
        // auto.
        assert!(!resolve_child_snapshot(false, true, false));
        assert!(!resolve_child_snapshot(false, true, true));
    }

    // ── read_project_version ──────────────────────────────────────────────

    #[test]
    fn read_project_version_returns_none_when_cargo_toml_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_project_version(tmp.path()), None);
    }

    #[test]
    fn read_project_version_reads_workspace_package_version() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            r#"[workspace]
members = ["crates/*"]

[workspace.package]
version = "1.2.3-test"
edition = "2021"
"#,
        )
        .unwrap();
        assert_eq!(
            read_project_version(tmp.path()),
            Some("1.2.3-test".to_string())
        );
    }

    #[test]
    fn read_project_version_reads_package_version() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            r#"[package]
name = "demo"
version = "0.4.2"
edition = "2021"
"#,
        )
        .unwrap();
        assert_eq!(read_project_version(tmp.path()), Some("0.4.2".to_string()));
    }

    #[test]
    fn read_project_version_prefers_workspace_when_both_present() {
        // Workspace inheritance: the root `[workspace.package].version`
        // is the authoritative version and `[package].version` is
        // usually `version.workspace = true`. When both literal values
        // are present we still prefer the workspace key because that's
        // what `cargo` itself would propagate via inheritance.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            r#"[workspace.package]
version = "9.9.9"

[package]
name = "root-crate"
version = "0.0.1"
"#,
        )
        .unwrap();
        assert_eq!(read_project_version(tmp.path()), Some("9.9.9".to_string()));
    }

    #[test]
    fn read_project_version_returns_none_on_malformed_toml() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "not valid \x00 toml ===").unwrap();
        assert_eq!(read_project_version(tmp.path()), None);
    }

    #[test]
    fn signature_suffix_extracts_literal_tail_after_last_expansion() {
        assert_eq!(
            signature_suffix("{{ .Artifact }}.cosign.bundle").as_deref(),
            Some(".cosign.bundle")
        );
        assert_eq!(
            signature_suffix("{{ .Artifact }}.sig").as_deref(),
            Some(".sig")
        );
        assert_eq!(
            signature_suffix("{{ .Artifact }}.asc").as_deref(),
            Some(".asc")
        );
    }

    #[test]
    fn signature_suffix_rejects_unanchorable_templates() {
        // A bare-expansion template (sign in place, no new extension) must
        // NOT yield a suffix — an empty tail would produce a `*` pattern
        // that allow-lists every artifact and suppresses all drift.
        assert_eq!(signature_suffix("{{ .Artifact }}"), None);
        assert_eq!(signature_suffix("{{ .Artifact }}   "), None);
        // Non-dotted tail can't anchor `*.<ext>`.
        assert_eq!(signature_suffix("{{ .Artifact }}sig"), None);
    }

    #[test]
    fn signature_allowlist_derives_custom_cosign_bundle_suffix() {
        use anodizer_core::config::{Config, SignConfig};
        // Mirrors cfgd: a checksum-signing cosign entry with a custom
        // `.cosign.bundle` signature template plus a default-`.sig` entry.
        let cfg = Config {
            signs: vec![
                SignConfig {
                    signature: Some("{{ .Artifact }}.cosign.bundle".into()),
                    ..Default::default()
                },
                SignConfig::default(), // default template → `.sig`
            ],
            ..Default::default()
        };
        let entries = signature_allowlist_entries_from_config(&cfg);
        let patterns: Vec<&str> = entries.iter().map(|e| e.artifact.as_str()).collect();
        assert!(
            patterns.contains(&"*.cosign.bundle"),
            "custom signature suffix must be allow-listed, got {patterns:?}"
        );
        assert!(patterns.contains(&"*.sig"), "got {patterns:?}");
        // Every derived pattern is a concrete extension anchor, never a
        // bare `*` (which would suppress all drift).
        assert!(entries.iter().all(|e| e.artifact != "*"));
    }

    /// Regression for the cfgd v0.4.0 determinism failure: the build was
    /// reproducible, but 18 signature/SBOM artifacts drifted and counted,
    /// failing the release. Every one of those exact names must now resolve
    /// to an allow-list reason through the canonical matcher — the SBOM
    /// documents via the compile-time list, the cosign bundles via the
    /// config-derived signature suffix.
    #[test]
    fn cfgd_v040_drift_set_is_fully_allowlisted() {
        use anodizer_core::DeterminismState;
        use anodizer_core::config::{Config, SignConfig};

        // cfgd's signing surface: a cosign checksum signer emitting
        // `.cosign.bundle`, plus default-`.sig` gpg/cosign entries.
        let cfg = Config {
            signs: vec![
                SignConfig {
                    signature: Some("{{ .Artifact }}.cosign.bundle".into()),
                    ..Default::default()
                },
                SignConfig::default(),
            ],
            binary_signs: vec![SignConfig::default()],
            ..Default::default()
        };

        let mut state = DeterminismState::seed_from_commit(0).expect("non-negative");
        for entry in signature_allowlist_entries_from_config(&cfg) {
            state.append_runtime(entry.artifact, entry.reason);
        }

        // The exact artifact set that drifted in run 26675983133.
        let drifted = [
            "cfgd-0.4.0-linux-amd64-installer.run.sha256.cosign.bundle",
            "cfgd-0.4.0-linux-amd64.tar.gz.cdx.json",
            "cfgd-0.4.0-linux-amd64.tar.gz.cdx.json.sha256",
            "cfgd-0.4.0-linux-amd64.tar.gz.cdx.json.sha256.cosign.bundle",
            "cfgd-0.4.0-linux-amd64.tar.gz.sha256.cosign.bundle",
            "cfgd-0.4.0-linux-arm64-installer.run.sha256.cosign.bundle",
            "cfgd-0.4.0-linux-arm64.tar.gz.cdx.json",
            "cfgd-0.4.0-linux-arm64.tar.gz.cdx.json.sha256",
            "cfgd-0.4.0-linux-arm64.tar.gz.cdx.json.sha256.cosign.bundle",
            "cfgd-0.4.0-linux-arm64.tar.gz.sha256.cosign.bundle",
            "cfgd-0.4.0-source.tar.gz.sha256.cosign.bundle",
            "cfgd_0.4.0_linux_amd64.apk.sha256.cosign.bundle",
            "cfgd_0.4.0_linux_amd64.deb.sha256.cosign.bundle",
            "cfgd_0.4.0_linux_amd64.rpm.sha256.cosign.bundle",
            "cfgd_0.4.0_linux_arm64.apk.sha256.cosign.bundle",
            "cfgd_0.4.0_linux_arm64.deb.sha256.cosign.bundle",
            "cfgd_0.4.0_linux_arm64.rpm.sha256.cosign.bundle",
            "install.sh.sha256.cosign.bundle",
            // macOS shard drift set (darwin universal + per-arch). NOTE:
            // `artifacts.json` is intentionally NOT here — it is no longer
            // blanket-allow-listed (that masked drift in gated members). The
            // determinism harness now judges it via the aggregate registry's
            // transitive-derivation rule, member by member.
            "cfgd-0.4.0-darwin-all.tar.gz.cdx.json",
            "cfgd-0.4.0-darwin-all.tar.gz.cdx.json.sha256",
            "cfgd-0.4.0-darwin-all.tar.gz.cdx.json.sha256.cosign.bundle",
            "cfgd-0.4.0-darwin-all.tar.gz.sha256.cosign.bundle",
            "cfgd-0.4.0-darwin-amd64.tar.gz.cdx.json",
            "cfgd-0.4.0-darwin-arm64.tar.gz.cdx.json.sha256.cosign.bundle",
            // cfgd-csi shard: a combined-checksums cosign bundle.
            "cfgd_0.4.0_checksums.txt.cosign.bundle",
        ];
        for name in drifted {
            assert!(
                state.resolve_reason(name).is_some(),
                "{name} drifted v0.4.0 and must now be allow-listed"
            );
        }

        // Negative control: a real build output must NOT be allow-listed,
        // so genuine binary drift still fails the harness.
        assert!(
            state
                .resolve_reason("cfgd-0.4.0-linux-amd64.tar.gz")
                .is_none(),
            "archive bytes must still be drift-checked"
        );
        assert!(
            state.resolve_reason("cfgd").is_none(),
            "raw binary must still be drift-checked"
        );
    }

    #[test]
    fn signature_allowlist_collects_per_workspace_signs() {
        use anodizer_core::config::{Config, SignConfig, WorkspaceConfig};
        let cfg = Config {
            workspaces: Some(vec![WorkspaceConfig {
                name: "member".into(),
                binary_signs: vec![SignConfig {
                    signature: Some("{{ .Artifact }}.bundle".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let patterns: Vec<String> = signature_allowlist_entries_from_config(&cfg)
            .into_iter()
            .map(|e| e.artifact)
            .collect();
        assert!(
            patterns.contains(&"*.bundle".to_string()),
            "per-workspace signature suffix must be collected, got {patterns:?}"
        );
    }
}
