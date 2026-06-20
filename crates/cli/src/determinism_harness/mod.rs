//! Determinism harness — drives N from-clean rebuilds in hermetic
//! worktrees and diffs the emitted artifacts.
//!
//! ## Shape
//!
//! ```ignore
//! let harness = Harness {
//!     repo_root, commit, stages, runs, sde, allowlist, report_path,
//! };
//! let report: DeterminismReport = harness.run()?;
//! ```
//!
//! The harness:
//!
//! 1. For each of `runs` runs, opens a fresh
//!    [`anodizer_core::git::worktree::Worktree`] rooted at `commit`.
//! 2. Builds an isolated env: per-run `CARGO_HOME`, `CARGO_TARGET_DIR`,
//!    `TMPDIR`, `HOME`; `SOURCE_DATE_EPOCH=self.sde`; `PATH` inherited
//!    from the host; plus an identity-only allow-list — see [`env`].
//! 3. Invokes the build-side pipeline (`anodize release --snapshot
//!    --skip=<SIDE_EFFECT_STAGES>`) inside the worktree with that env.
//! 4. Walks `<worktree>/dist` AND `<worktree>/.det-tmp/target/`,
//!    SHA256s every file, returns a `BTreeMap<artifact_name, info>`
//!    for the run.
//! 5. Once all runs complete, diffs the maps and constructs a
//!    [`anodizer_core::DeterminismReport`]. Allow-listed artifacts (the
//!    compile-time + runtime lists carried on `self.allowlist`) are
//!    excluded from `drift_count` but still appear in `artifacts` and
//!    (with per-run hashes) in `drift`.
//!
//! ## Implementation choice: shell to `current_exe`
//!
//! The harness shells out to the currently-running `anodizer` binary
//! rather than calling [`crate::pipeline::build_release_pipeline`]
//! directly. Rationale: a) `Context` setup in-process requires re-parsing
//! the config + re-deriving the SDE + reconciling all the global flags,
//! reproducing logic that already lives in `main.rs`; b) shelling out
//! gives true env isolation (we can `env_clear` on the child without
//! touching the harness process); c) the binary on disk is what the
//! release pipeline ships, so byte-stability of *that* binary is what we
//! actually want to assert.
//!
//! ## Allow-list semantics
//!
//! Allow-list matching uses the same `*.ext` glob semantics as
//! [`anodizer_core::DeterminismState::resolve_reason`]: a leading `*` is
//! a suffix-match, anything else is exact-match. Compile-time matches
//! win on collision; the matched reason populates
//! [`ArtifactRow::nondeterministic_reason`].

mod artifacts;
mod drift;
mod env;
mod installer_detect;
mod preserve;

pub use installer_detect::installer_stages;

use anodizer_core::git::worktree::Worktree;
use anodizer_core::harness_signing::EphemeralSigningKeys;
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::{AllowList, ArtifactRow, CURRENT_SCHEMA_VERSION, DeterminismReport, DriftRow};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use artifacts::{
    ArtifactInfo, copy_artifacts_to_dump, discover_artifacts, hash_artifacts, prune_dump_to_drifted,
};
use drift::{inject_drift_byte, pick_first_artifact_for_stage, summarize_drift};
use env::{BuildSubprocessEnv, build_subprocess_env};
use preserve::{
    ContextInputs, preserve_dist_tree, preserve_raw_binaries, remove_preserved_on_drift,
    write_preserved_dist_context,
};

/// Stage subset selector for `--stages=<subset>`.
///
/// Currently informational: every variant maps to "run the build-side
/// pipeline and look at the artifacts that stage produces". The harness
/// shells to `anodize release --snapshot --skip=...` which runs the full
/// build-side pipeline; finer-grained per-stage gating is a follow-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StageId {
    Build,
    Source,
    Upx,
    Archive,
    Nfpm,
    Makeself,
    Snapcraft,
    Sbom,
    Sign,
    Checksum,
    /// `cargo package` byte-stability probe.
    ///
    /// Harness-only stage; not part of `build_release_pipeline`. When
    /// listed in `--stages=cargo-package`, the harness runs
    /// `cargo package --no-verify --allow-dirty` for every crate
    /// declared in `.anodizer.yaml` (or the workspace root when none
    /// are declared) per run, hashes the resulting `.crate` tarballs,
    /// and diffs them across runs.
    ///
    /// Why a dedicated stage: `cargo publish` (the production path)
    /// packages internally then uploads. The packaging step has a
    /// long-running non-determinism story (file mtimes,
    /// `.cargo_vcs_info.json` contents, tar member ordering); cargo
    /// has fixed most of these since 1.74 but the harness needs to
    /// detect regressions in the project's packaging stack and pin
    /// any remaining sources.
    ///
    /// Known non-determinism the workaround set addresses:
    /// - **File mtimes inside the tar**: `SOURCE_DATE_EPOCH` is
    ///   exported via [`super::env::build_subprocess_env`] and cargo
    ///   canonicalizes mtimes to it since 1.74.
    /// - **tar member ordering**: cargo sorts entries since 1.74; no
    ///   per-call workaround required.
    /// - **`.cargo_vcs_info.json`**: cargo writes the git sha + dirty
    ///   flag; the harness's per-run worktree is detached at the same
    ///   commit, so the sha is stable. The dirty flag depends on
    ///   whether the worktree was perturbed before packaging; for a
    ///   fresh `git worktree add --detach <tmp> <commit>` it should
    ///   read `false`.
    ///
    /// Any drift detected after the workarounds is a real regression
    /// in cargo's packaging reproducibility — surface the diff in the
    /// report and don't silently pass.
    CargoPackage,
    /// Docker BuildKit reproducibility probe.
    ///
    /// Harness-only stage; not part of `build_release_pipeline` (the
    /// production `docker` stage is on
    /// [`anodizer_core::determinism_runner::SIDE_EFFECT_STAGES`] because
    /// it talks to a daemon and pushes to registries — neither belongs
    /// in a hermetic rebuild). When listed in `--stages=docker`, the
    /// harness invokes
    /// `docker buildx build --output=type=oci,rewrite-timestamp=true,dest=…`
    /// against the `Dockerfile` at the worktree root per run, emits an
    /// OCI tarball to disk, SHA-256s the tarball, and records the
    /// BuildKit-reported image digest. Both fingerprints must match
    /// across runs for the stage to greenlight.
    ///
    /// Known non-determinism the workaround set addresses:
    /// - **File mtimes inside layers**: `rewrite-timestamp=true` on the
    ///   OCI exporter rewrites every layer-entry mtime to
    ///   `SOURCE_DATE_EPOCH` (BuildKit ≥ 0.13). The harness exports
    ///   `SOURCE_DATE_EPOCH` via [`super::env::build_subprocess_env`].
    /// - **Provenance + SBOM attestations**: both are disabled
    ///   (`--provenance=false --sbom=false`) since their bodies embed
    ///   wall-clock timestamps and BuildKit version strings.
    /// - **Cosign signature timestamps**: out of scope for this stage.
    ///   The production sign path uploads transparency-log entries by
    ///   default (`cosign sign <image>`), and those embed signing
    ///   timestamps that are non-deterministic by design. A future
    ///   harness mode that signs the OCI tarball would have to pass
    ///   `--tlog-upload=false` to opt out of transparency for byte-
    ///   stable signatures.
    ///
    /// Skipped (no drift, no artifact rows) when the worktree has no
    /// `Dockerfile` at its root — keeps the stage harmless for repos
    /// that wire docker via config but didn't bootstrap one. Skipped
    /// (with a one-line warning through the harness logger) when
    /// `docker buildx` is not reachable on the host so the harness
    /// stays usable on machines without Docker installed.
    Docker,
    /// MSI installer reproducibility probe (Windows).
    ///
    /// Drives the `stage-msi` crate via the child release subprocess
    /// (`wix` / `candle` / `light` invocations live inside the stage's
    /// allow-listed spawn surface). Skipped at the harness gate when
    /// `wix` is not on `PATH`, so the harness stays usable on Linux /
    /// macOS hosts that lack the WiX toolset.
    Msi,
    /// NSIS installer reproducibility probe (Windows).
    ///
    /// Drives the `stage-nsis` crate. The `makensis` binary is the
    /// gating tool; skipped when missing so the harness stays usable
    /// on hosts without NSIS installed.
    Nsis,
    /// DMG installer reproducibility probe (macOS).
    ///
    /// Drives the `stage-dmg` crate. Primary tool is `hdiutil` on
    /// macOS and `mkisofs` on Linux (matching how the stage's spawn
    /// surface picks its DMG-builder). Skipped when the primary tool
    /// is missing on the host.
    Dmg,
    /// `.pkg` installer reproducibility probe (macOS).
    ///
    /// Drives the `stage-pkg` crate via the child release subprocess
    /// (`pkgbuild` / `productbuild` invocations live inside the
    /// stage's allow-listed spawn surface). Skipped when `pkgbuild`
    /// is not on `PATH`.
    Pkg,
    /// Source RPM (`.src.rpm`) reproducibility probe.
    ///
    /// Drives the `stage-srpm` crate. Primary tool is `rpmbuild`;
    /// skipped when missing so the harness stays usable on macOS /
    /// Windows hosts that lack RPM tooling.
    Srpm,
    /// macOS `.app` bundle reproducibility probe.
    ///
    /// Drives the `stage-appbundle` crate, which is pure file assembly
    /// (Info.plist + binary copy) with no external tool, so it builds on
    /// any host. No gating tool — always available. Present in the stage
    /// vocabulary so a `--stages=` list can keep it out of the harness's
    /// child `--skip=` complement; without it, a `dmg`/`pkg` stage with
    /// `use: appbundle` would find no appbundle artifact and silently
    /// produce nothing during a determinism run.
    Appbundle,
}

impl StageId {
    /// Lowercase canonical name, matching `--stages=` CLI tokens and the
    /// `stages_under_test` array in the report.
    pub fn as_str(self) -> &'static str {
        match self {
            StageId::Build => "build",
            StageId::Source => "source",
            StageId::Upx => "upx",
            StageId::Archive => "archive",
            StageId::Nfpm => "nfpm",
            StageId::Makeself => "makeself",
            StageId::Snapcraft => "snapcraft",
            StageId::Sbom => "sbom",
            StageId::Sign => "sign",
            StageId::Checksum => "checksum",
            StageId::CargoPackage => "cargo-package",
            StageId::Docker => "docker",
            StageId::Msi => "msi",
            StageId::Nsis => "nsis",
            StageId::Dmg => "dmg",
            StageId::Pkg => "pkg",
            StageId::Srpm => "srpm",
            StageId::Appbundle => "appbundle",
        }
    }
}

/// Preamble stages the child release subprocess MUST keep enabled
/// regardless of `--stages=`. These don't produce per-target artifacts
/// the harness diffs, but the pipeline needs them to function:
///
/// - `validate` — config / target / signing-cred validation.
/// - `before` — user `before:` hooks (e.g. codegen).
/// - `templatefiles` — pre-build template materialization.
///
/// Adding any of these to the child `--skip=` list would break stages
/// that depend on their side-effects-on-context (not on disk), which is
/// why the harness's complement-set calculation subtracts them.
const PRESERVE_SET: &[&str] = &["validate", "before", "templatefiles"];

/// Compute the harness's child-subprocess "extra skip" set — every stage
/// name in [`anodizer_core::context::VALID_RELEASE_SKIPS`] that is NOT:
///
/// - in the operator's requested-stages list (`requested`), OR
/// - in [`PRESERVE_SET`] (preamble helpers the pipeline needs), OR
/// - already in
///   [`anodizer_core::determinism_runner::SIDE_EFFECT_STAGES`] (the
///   runner merges those in unconditionally; subtracting them here just
///   keeps the returned list lean).
///
/// Why this matters: `--stages=` is the harness's "what to diff" filter,
/// but it does NOT restrict which stages the child release subprocess
/// runs. Without this complement set the child runs the full pipeline
/// (minus side-effects), including produce-stages like `nfpm`, `nsis`,
/// `msi`, `dmg`, `pkg`, `snapcraft`, `source`, `flatpak`, `appbundle`,
/// `srpm`, `upx`, `makeself`, `notarize`. On macOS / Windows shards
/// those binaries aren't installed; on Linux shards some are but the
/// target artifacts don't exist on a non-native shard.
fn compute_extra_skip(requested: &[StageId]) -> Vec<String> {
    use anodizer_core::context::VALID_RELEASE_SKIPS;
    use anodizer_core::determinism_runner::SIDE_EFFECT_STAGES;
    let requested_names: BTreeSet<&str> = requested.iter().map(|s| s.as_str()).collect();
    VALID_RELEASE_SKIPS
        .iter()
        .copied()
        .filter(|name| !requested_names.contains(name))
        .filter(|name| !PRESERVE_SET.contains(name))
        .filter(|name| !SIDE_EFFECT_STAGES.contains(name))
        .map(str::to_string)
        .collect()
}

/// Glob match copy-paste from `anodizer_core::determinism` (kept local
/// to avoid exposing that helper publicly; the determinism module owns
/// the canonical semantics). `*.ext` is suffix-match; anything else is
/// exact-match.
fn matches_artifact_pattern(pattern: &str, artifact: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        return artifact.ends_with(suffix);
    }
    pattern == artifact
}

/// Stage a docker build context that mirrors the production `docker`
/// stage's layout so a `docker buildx build` against it resolves the
/// repo Dockerfile's `COPY ${TARGETOS}/${TARGETARCH}/${BIN}`.
///
/// Reference layout: `anodizer_stage_docker`'s `stage_artifacts_v2`
/// (`<context>/<os>/<arch>/<name>`) + `copy_dockerfile`
/// (`<context>/Dockerfile`). The real stage stages from a loaded
/// `Context`'s artifacts; the harness has no `Context` (it ran the
/// release as a subprocess), so it stages from the per-triple binaries
/// it discovered on disk, mapping each triple → `(os, arch)` via the
/// same [`anodizer_core::target::map_target`] helper the real stage uses.
///
/// Returns the number of binaries staged. Zero means the build produced
/// no per-triple binaries — the caller forks on `explicitly_requested`
/// rather than spawn a build whose `COPY` is guaranteed to fail.
///
/// `context_dir` is wiped first (mirroring the defensive cleanup in
/// [`anodizer_core::docker_build::oci_build_fixture`]) so a re-run can't
/// carry stale bytes from a prior run's staging.
fn stage_docker_context(
    worktree_path: &Path,
    context_dir: &Path,
    dockerfile: &Path,
    log: &StageLogger,
) -> Result<usize> {
    use anodizer_core::target::map_target;

    let _ = std::fs::remove_dir_all(context_dir);
    std::fs::create_dir_all(context_dir)
        .with_context(|| format!("creating docker staging dir {}", context_dir.display()))?;

    let mut staged = 0usize;
    for (triple, bin_path) in discover_per_triple_binaries(worktree_path)? {
        let (os, arch) = map_target(&triple);
        let file_name = bin_path
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .unwrap_or_else(|| std::ffi::OsString::from("anodizer"));
        let dest_dir = context_dir.join(&os).join(&arch);
        std::fs::create_dir_all(&dest_dir)
            .with_context(|| format!("creating staging platform dir {}", dest_dir.display()))?;
        let dest = dest_dir.join(&file_name);
        std::fs::copy(&bin_path, &dest)
            .with_context(|| format!("staging {} → {}", bin_path.display(), dest.display()))?;
        log.verbose(&format!(
            "staged {} → {}",
            bin_path.display(),
            dest.display()
        ));
        staged += 1;
    }

    let dockerfile_dest = context_dir.join("Dockerfile");
    std::fs::copy(dockerfile, &dockerfile_dest).with_context(|| {
        format!(
            "staging Dockerfile {} → {}",
            dockerfile.display(),
            dockerfile_dest.display()
        )
    })?;

    Ok(staged)
}

/// Discover the per-triple release binaries under
/// `<worktree>/.det-tmp/target/<triple>/release/<bin>` as
/// `(triple, binary_path)` pairs.
///
/// Only the per-triple builds are surfaced — the bare host
/// `release/<bin>` is a non-shipped tooling byproduct (the man-page
/// `before:` hook's `cargo run`) and never lands in an image. The file
/// filter matches [`artifacts::discover_artifacts`]: regular files with
/// an empty extension (`anodizer`) or `.exe` (`anodizer.exe`).
fn discover_per_triple_binaries(worktree_path: &Path) -> Result<Vec<(String, PathBuf)>> {
    let target_root = worktree_path.join(".det-tmp").join("target");
    let entries = match std::fs::read_dir(&target_root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", target_root.display())),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let triple = name.to_string_lossy();
        // Skip the bare host `release/` dir and cargo's scratch dirs;
        // only `<triple>/release/` directories ship into images.
        if triple == "release" || triple == "debug" || triple.starts_with('.') {
            continue;
        }
        let release_dir = entry.path().join("release");
        if !release_dir.is_dir() {
            continue;
        }
        for bin in std::fs::read_dir(&release_dir)
            .with_context(|| format!("reading {}", release_dir.display()))?
        {
            let bin = bin?;
            if !bin.file_type()?.is_file() {
                continue;
            }
            let path = bin.path();
            match path.extension().and_then(|s| s.to_str()) {
                None | Some("exe") => out.push((triple.to_string(), path)),
                _ => continue,
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Harness configuration. Constructed by the CLI dispatcher
/// (`crate::commands::check::determinism::run`) and consumed once via
/// [`Harness::run`].
pub struct Harness {
    /// Repository root that owns the worktrees the harness will spawn.
    pub repo_root: PathBuf,
    /// Full commit SHA the harness rebuilds. Each run does
    /// `git worktree add --detach <tmp> <commit>`.
    pub commit: String,
    /// Stage subset under test. Surfaced into the report's
    /// `stages_under_test` field.
    pub stages: Vec<StageId>,
    /// Number of from-clean rebuilds to perform.
    pub runs: u32,
    /// `SOURCE_DATE_EPOCH` value to export into every run's subprocess
    /// env. Resolved by the CLI dispatcher (snapshot resolver under
    /// `--snapshot`, HEAD commit timestamp otherwise).
    pub sde: i64,
    /// Compile-time + runtime allow-lists used to exclude artifacts from
    /// `drift_count` (entries still appear in `artifacts` and `drift`).
    pub allowlist: AllowList,
    /// Destination path for the JSON report. The CLI dispatcher owns
    /// writing the file; the harness uses the parent dir as the root
    /// for the drift-bins dump.
    pub report_path: PathBuf,
    /// `--inject-drift=<stage>` (test-harness gated): after each run
    /// completes, append one random byte to the first artifact whose
    /// inferred stage equals this value. Forces the harness to detect
    /// drift across runs so integration tests can verify the report
    /// shape on the failure path. `None` outside the
    /// `ANODIZE_TEST_HARNESS=1` env (rejected upstream by the CLI
    /// dispatcher).
    pub inject_drift: Option<String>,
    /// `--targets=<csv>`: restrict the harness to a subset of configured
    /// target triples. Forwarded to the child `anodize release
    /// --snapshot` subprocess as `--targets=<csv>` so the rebuild only
    /// touches buildable targets on this runner. `None` validates every
    /// configured target.
    pub targets: Option<Vec<String>>,
    /// `--preserve-dist=<path>`: when set AND `drift_count == 0`, copy
    /// `<worktree>/dist/**` from the first run to this path before the
    /// worktree is destroyed, then emit a `context.json` manifest
    /// describing the preserved artifact set. Consumed by the release
    /// workflow's publish-only flow so the determinism step's output
    /// can be shipped directly without a redundant rebuild.
    ///
    /// The copy happens at the end of run 0 (run-0 and run-N are
    /// byte-identical by construction once the harness passes; run-0 is
    /// picked deterministically). If the harness later detects drift
    /// across runs, the preserved directory is removed so shippable
    /// bytes never escape a failed determinism check.
    pub preserve_dist: Option<PathBuf>,
    /// Fallback version string used in `context.json` when the
    /// preserved-dist's `metadata.json` is missing or malformed.
    /// Dispatcher resolves this from the snapshot template variables
    /// (or `Cargo.toml` for non-snapshot runs) so the manifest's
    /// `version` field is non-empty even when the sibling JSON
    /// vanishes. Pass an empty string to keep the prior behaviour
    /// (manifest `version` empty when JSON missing).
    ///
    /// Unused when `preserve_dist` is `None`.
    pub version_hint: String,
    /// Whether to pass `--snapshot` to the child `anodize release ...`
    /// subprocess. `true` (the default / legacy behaviour) emits
    /// artifacts named with the snapshot version suffix
    /// (`-SNAPSHOT-<sha>`); `false` drops the flag so produce-stages
    /// emit artifacts named with the actual release version. The
    /// release workflow flips this off on tag-push runs so the bytes
    /// preserved by `--preserve-dist` are immediately shippable via
    /// `anodize release --publish-only`.
    pub child_snapshot: bool,
    /// Operator-selected output verbosity (global `--quiet` /
    /// `--verbose` / `--debug` flags). Drives the harness's own logger
    /// (the `run N of M` bullets) and is forwarded to each child
    /// `anodize release` subprocess so the whole interleaved stream
    /// honors one verbosity contract.
    pub verbosity: Verbosity,
    /// Backend hint forwarded from the CLI dispatcher's reading of the
    /// project's `dockers_v2[*].use` field. `Some("podman")` causes
    /// [`Harness::run_docker_stage`] to short-circuit with an explanatory
    /// warning — the harness's reproducibility probe shells out to
    /// `docker buildx`, and BuildKit-only flags such as `--rewrite-timestamp`
    /// and `--output=type=oci,...` are not recognised by plain `podman
    /// build`. Operators who want byte-stability for podman-built images
    /// must verify reproducibility outside the harness (the build path
    /// itself stays canonical).
    ///
    /// `None` / `Some("buildx")` / `Some("docker")` preserve the historical
    /// behaviour of always invoking `docker buildx build`.
    pub docker_backend_hint: Option<String>,
    /// When set alongside `preserve_dist`, the preserved dist tree is
    /// written to `<preserve_dist>/<crate_name>/` rather than directly
    /// into `<preserve_dist>/`. This prevents context.json collision when
    /// multiple crates release in parallel and their dist trees are merged
    /// by `download-artifact merge-multiple: true`.
    ///
    /// When `preserve_dist` is `None`, this field has no effect.
    pub crate_name: Option<String>,
}

impl Harness {
    /// Drive the harness end-to-end and return the populated report.
    ///
    /// Does NOT write the report — the CLI dispatcher is responsible for
    /// serializing the returned `DeterminismReport` and exiting non-zero
    /// when `drift_count > 0`.
    pub fn run(&self) -> Result<DeterminismReport> {
        let mut per_run_hashes: Vec<BTreeMap<String, ArtifactInfo>> =
            Vec::with_capacity(self.runs as usize);

        // Preserve-dist + production-keys → skip Sign in the harness.
        //
        // When the workflow plans to ship the harness's output via the
        // publish-only path (`--preserve-dist=<path>` set on the harness;
        // `COSIGN_KEY` / `GPG_PRIVATE_KEY` exported on the runner), the
        // harness's ephemeral signatures would land in the preserved dist
        // and have to be stripped before re-signing with production keys.
        // Cleaner to never write them: skip the Sign stage entirely.
        //
        // KNOWN COVERAGE GAP: byte-stability of the Sign stage is no
        // longer exercised in CI when this branch fires. Acceptable
        // tradeoff — the `harness_signing` unit tests already pin the
        // SDE-based key derivation (cosign-keygen + GPG `--faked-system-
        // time`) so the deterministic-keys property has direct coverage,
        // and the production sign stage is exercised by every release.
        let skip_sign_for_preserve = self.preserve_dist.is_some()
            && (std::env::var_os("COSIGN_KEY").is_some()
                || std::env::var_os("GPG_PRIVATE_KEY").is_some());
        let effective_stages: Vec<StageId> = if skip_sign_for_preserve {
            self.stages
                .iter()
                .copied()
                .filter(|s| *s != StageId::Sign)
                .collect()
        } else {
            self.stages.clone()
        };

        // Installer-tool availability gate: drop any installer-family
        // stage whose primary tool is not on PATH. The pipeline would
        // otherwise fail at `Command::new("wix")` / `Command::new("rpmbuild")`
        // mid-run, surfacing as a confusing build error instead of an
        // honest "tool absent → stage skipped". Non-installer stages
        // pass through unmodified (sign / docker / cargo-package have
        // their own gates downstream).
        let gate = installer_detect::filter_available_installer_stages(&effective_stages);
        let effective_stages = gate.available;
        // Routed through the harness logger (not a bare eprintln) so
        // `-q` silences these like every other harness line.
        let warn_log = StageLogger::new("check-determinism", self.verbosity);
        for (stage, tool) in &gate.skipped {
            warn_log.warn(&format!(
                "skipped installer stage `{}` for this run — `{}` is not on PATH \
                 (no artifacts emitted)",
                stage.as_str(),
                tool
            ));
        }

        // Provision once: both runs must sign with identical key
        // material, otherwise even byte-deterministic GPG signatures
        // would diverge. Skipped when `skip_sign_for_preserve` is set
        // (no Sign stage → no keys needed).
        let signing_keys: Option<EphemeralSigningKeys> =
            if effective_stages.contains(&StageId::Sign) {
                Some(anodizer_core::harness_signing::provision_ephemeral_keys(
                    self.sde,
                )?)
            } else {
                None
            };

        // Default to <repo_root>/.det-worktrees/ — keeps the harness
        // off `/tmp` (which is tmpfs on many distros and exhausts fast
        // when the cargo target dir lives inside the worktree). CI
        // (GitHub Actions) sets RUNNER_TEMP to a disk-backed path
        // outside the repo, so honor that when present.
        let worktree_root = std::env::var_os("RUNNER_TEMP")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.repo_root.join(".det-worktrees"));
        let _ = std::fs::create_dir_all(&worktree_root);
        // PID-suffix the worktree so parallel harness invocations
        // (cargo test running multiple determinism integration tests
        // concurrently) don't collide on the same path. WITHIN one
        // invocation every run reuses the same path — that's the
        // load-bearing invariant for /Brepro and UTF-16 cargo-registry
        // paths embedded into binaries (drift otherwise cascades from
        // a 2-byte path diff). Across invocations the path must be
        // unique because git worktree add refuses a populated target.
        let worktree_path =
            worktree_root.join(format!("anodize-determinism-{}", std::process::id()));

        // When crate_name is set, anchor the preserved dist into a
        // per-crate subdir so parallel crate releases (each invoking
        // the harness independently) can merge into one `dist/` root
        // without colliding on `context.json` / `artifacts.json`. All
        // downstream writers (preserve_dist_tree, preserve_raw_binaries,
        // write_preserved_dist_context) accept this dest directly and
        // emit into it as-is; the subdir is computed once here so the
        // path stays consistent across the three calls below.
        let effective_preserve_dest: Option<std::path::PathBuf> =
            self.preserve_dist.as_ref().map(|base| {
                if let Some(ref name) = self.crate_name {
                    base.join(name)
                } else {
                    base.clone()
                }
            });

        // Emits the per-run delimiter bullets inside the dispatcher's
        // `Checking determinism` section; the child subprocess's own
        // sections nest beneath each bullet via the inherited log depth
        // (see `determinism_runner::build_subprocess_command`).
        let log = StageLogger::new("check-determinism", self.verbosity);

        for run_idx in 0..self.runs {
            log.detail(&format!("run {} of {}", run_idx + 1, self.runs));
            // Defensive: prior aborted runs may have left the dir behind;
            // `git worktree add` would reject a populated target.
            let _ = std::fs::remove_dir_all(&worktree_path);
            let worktree = Worktree::add(&self.repo_root, &worktree_path, &self.commit)
                .with_context(|| format!("creating worktree for determinism run {}", run_idx))?;
            let env = self.build_isolated_env(&worktree, signing_keys.as_ref())?;
            self.run_build_pipeline(worktree.path(), &env, &effective_stages)
                .with_context(|| format!("building pipeline for determinism run {}", run_idx))?;
            if effective_stages.contains(&StageId::CargoPackage) {
                self.run_cargo_package(worktree.path(), &env)
                    .with_context(|| {
                        format!(
                            "running cargo-package stage for determinism run {}",
                            run_idx
                        )
                    })?;
            }
            if effective_stages.contains(&StageId::Docker) {
                // `docker` is never auto-included: it is absent from the
                // parser's default set and from the `installers` umbrella,
                // so its presence here means the operator typed it. A gate
                // that silently skips an explicitly-requested stage is
                // false coverage, hence the hard-error skip contract below.
                let docker_explicitly_requested = self.stages.contains(&StageId::Docker);
                self.run_docker_stage(worktree.path(), &env, docker_explicitly_requested)
                    .with_context(|| {
                        format!("running docker stage for determinism run {}", run_idx)
                    })?;
            }
            let artifacts = discover_artifacts(worktree.path())?;
            // `--inject-drift=<stage>` (test-harness gated): mutate the
            // first artifact of the named stage before hashing so the
            // report records drift. The miss path logs the discovered
            // artifact set so a silent "found no matching stage" in CI
            // is debuggable from logs alone.
            if let Some(stage) = self.inject_drift.as_deref() {
                match pick_first_artifact_for_stage(&artifacts, stage) {
                    Some(victim) => {
                        inject_drift_byte(victim).with_context(|| {
                            format!(
                                "injecting drift byte into {} on run {}",
                                victim.display(),
                                run_idx
                            )
                        })?;
                    }
                    None => {
                        let summary: Vec<String> = artifacts
                            .iter()
                            .map(|p| {
                                let s = p.to_string_lossy();
                                format!(
                                    "  {} → {}",
                                    p.display(),
                                    artifacts::infer_stage_from_path(&s)
                                )
                            })
                            .collect();
                        StageLogger::new("check-determinism", self.verbosity).warn(&format!(
                            "--inject-drift={} matched no artifact on run {}; \
                             discovered artifacts ({}):\n{}",
                            stage,
                            run_idx,
                            artifacts.len(),
                            summary.join("\n")
                        ));
                    }
                }
            }
            per_run_hashes.push(hash_artifacts(worktree.path(), &artifacts)?);
            // Copy every artifact to a per-run dump directory under the
            // report's parent. This is the diagnostic escape hatch:
            // when drift is detected, the full binaries are uploaded
            // alongside the JSON report so root-causing residual
            // non-determinism doesn't depend on re-running the harness.
            // Non-drifted entries are pruned after the comparison
            // below so the artifact zip stays compact.
            if let Some(parent) = self.report_path.parent() {
                let dump_root = parent.join("drift-bins").join(format!("run-{}", run_idx));
                copy_artifacts_to_dump(worktree.path(), &artifacts, &dump_root, &log)
                    .with_context(|| {
                        format!(
                            "dumping artifacts to {} for determinism run {}",
                            dump_root.display(),
                            run_idx
                        )
                    })?;
            }
            // Preserve run-0's dist tree to the operator-supplied path
            // BEFORE the next iteration's `remove_dir_all` (or this
            // iteration's `Worktree::drop`) wipes it. run-0 is the
            // earliest deterministic pick — runs 1..N are byte-identical
            // to run-0 once the harness passes, but the next run's
            // `remove_dir_all` at the top of the loop deletes the
            // worktree wholesale, so we copy from run-0 specifically.
            //
            // The drift gate happens POST-loop: if drift is detected
            // after all runs finish, we delete the preserved dir below
            // so shippable bytes never escape a failed determinism run.
            if run_idx == 0
                && let Some(dest) = effective_preserve_dest.as_ref()
            {
                preserve_dist_tree(worktree.path(), dest).with_context(|| {
                    format!(
                        "preserving run-0 dist tree from {} to {}",
                        worktree.path().join("dist").display(),
                        dest.display()
                    )
                })?;
                // Mirror raw cargo binaries under `<dest>/bin/<triple>/`
                // and rewrite their paths in `<dest>/artifacts.json` so
                // publish-only's `SignStage` can resolve them under the
                // preserved tree (binaries live outside `dist/` in the
                // worktree and are otherwise lost when the worktree is
                // dropped).
                preserve_raw_binaries(worktree.path(), dest, &log).with_context(|| {
                    format!(
                        "preserving raw binaries from {} into {}",
                        worktree.path().display(),
                        dest.display()
                    )
                })?;
                // No `preserved_dist_filled` flag needed: any error in
                // preserve_dist_tree propagates via `?` and aborts the
                // harness before the post-loop block runs. Reaching
                // post-loop with `self.preserve_dist == Some(_)` is
                // sufficient proof the copy succeeded.
            }
            // Worktree dropped at end of scope → cleanup automatic.
        }

        let report = self.build_report(per_run_hashes);
        if let Some(parent) = self.report_path.parent() {
            prune_dump_to_drifted(&parent.join("drift-bins"), &report);
        }
        // Preserve-dist gate. Restructured per code review: if any
        // copy failed mid-loop the `?` propagation already aborted the
        // harness, so reaching this point with
        // `self.preserve_dist == Some(_)` means run-0's tree IS on
        // disk under `dest`. Branch on drift_count alone.
        //
        // Safety property: shippable bytes must come from a green
        // determinism run, never a drifted one. Drift → remove the
        // tree; green → write `<dest>/context.json` so the publish-
        // only path can rehydrate.
        if let Some(dest) = effective_preserve_dest.as_ref() {
            if report.drift_count > 0 {
                remove_preserved_on_drift(dest, &log);
            } else {
                write_preserved_dist_context(
                    dest,
                    ContextInputs {
                        report: &report,
                        harness_targets: self.targets.as_deref(),
                        version_hint: &self.version_hint,
                    },
                    &log,
                )
                .with_context(|| {
                    format!(
                        "writing context.json under preserved dist {}",
                        dest.display()
                    )
                })?;
            }
        }
        Ok(report)
    }

    /// Construct the env map handed to each child build process.
    fn build_isolated_env(
        &self,
        worktree: &Worktree,
        signing_keys: Option<&EphemeralSigningKeys>,
    ) -> Result<HashMap<String, String>> {
        let tmpdir = worktree.path().join(".det-tmp");
        std::fs::create_dir_all(&tmpdir)?;
        let cargo_home = tmpdir.join("cargo");
        let cargo_target = tmpdir.join("target");
        let home_dir = tmpdir.join("home");
        std::fs::create_dir_all(&cargo_home)?;
        std::fs::create_dir_all(&home_dir)?;

        Ok(build_subprocess_env(&BuildSubprocessEnv {
            cargo_home: &cargo_home,
            cargo_target: &cargo_target,
            tmpdir: &tmpdir,
            home_dir: &home_dir,
            sde: self.sde,
            worktree: worktree.path(),
            signing_keys,
        }))
    }

    /// Shell to the running `anodize` binary inside the worktree.
    ///
    /// Delegates to [`anodizer_core::determinism_runner`] — `crates/cli/**`
    /// is on the forbid-list for direct subprocess spawn, so the actual
    /// `Command::new` lives in core where it's allow-listed.
    ///
    /// `effective_stages` is what the harness actually ran the child
    /// pipeline against — usually equal to `self.stages`, but with
    /// `Sign` filtered out when [`Harness::preserve_dist`] is set AND
    /// production signing keys are present on the runner (so the harness
    /// doesn't leave ephemeral sigs in the preserved dist; they would
    /// only get stripped + re-signed later anyway).
    fn run_build_pipeline(
        &self,
        worktree_path: &Path,
        env: &HashMap<String, String>,
        effective_stages: &[StageId],
    ) -> Result<()> {
        let exe = anodizer_core::determinism_runner::current_anodize_binary()?;
        let extra_skip = compute_extra_skip(effective_stages);
        anodizer_core::determinism_runner::run_build_pipeline_subprocess(
            &anodizer_core::determinism_runner::ChildInvocation {
                anodize_binary: &exe,
                worktree_path,
                env,
                targets: self.targets.as_deref(),
                extra_skip: &extra_skip,
                snapshot: self.child_snapshot,
                crate_name: self.crate_name.as_deref(),
                verbosity: self.verbosity,
            },
        )
    }

    /// Drive the `cargo-package` stage when [`StageId::CargoPackage`] is
    /// in the requested stage set.
    ///
    /// Delegates to [`anodizer_core::cargo_package::package_workspace`]
    /// (the allow-listed subprocess entry point), then copies the
    /// emitted `<cargo_target>/package/*.crate` into
    /// `<worktree>/dist/cargo-package/` so the existing
    /// [`discover_artifacts`] walker picks them up under the normal
    /// `dist/` surface.
    ///
    /// `SOURCE_DATE_EPOCH` is already in `env` — the harness exports it
    /// from [`Harness::sde`] via [`super::env::build_subprocess_env`].
    /// cargo (≥ 1.74) canonicalizes mtimes inside the `.crate` tar to
    /// the supplied epoch and sorts tar members alphabetically, which
    /// covers the two leading non-determinism sources. Residual drift
    /// (`.cargo_vcs_info.json` contents, registry path embedding,
    /// future cargo regressions) will appear in the report's `drift`
    /// section instead of silently passing.
    fn run_cargo_package(&self, worktree_path: &Path, env: &HashMap<String, String>) -> Result<()> {
        let log = StageLogger::new("check-determinism", self.verbosity);
        anodizer_core::cargo_package::package_workspace(worktree_path, env, &log)?;
        // cargo writes to `<cargo_target>/package/<name>-<version>.crate`
        // where `cargo_target` came from `CARGO_TARGET_DIR` in the env
        // block. The env block sets `CARGO_TARGET_DIR=<worktree>/.det-tmp/target`
        // so the .crate files land there.
        let source = worktree_path
            .join(".det-tmp")
            .join("target")
            .join("package");
        let dest = worktree_path.join("dist").join("cargo-package");
        std::fs::create_dir_all(&dest)
            .with_context(|| format!("creating dest dir {}", dest.display()))?;
        if !source.exists() {
            // No `.crate` files emitted (e.g. workspace virtual manifest
            // with no `[package]` member). Treat as a no-op so the harness
            // doesn't fail when an operator points it at a virtual
            // workspace by mistake — the resulting drift report will be
            // empty for the cargo-package stage, which correctly reflects
            // "nothing exercised".
            return Ok(());
        }
        for entry in
            std::fs::read_dir(&source).with_context(|| format!("reading {}", source.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("crate") {
                let name = path
                    .file_name()
                    .with_context(|| format!("crate path lacks filename: {}", path.display()))?;
                let target = dest.join(name);
                std::fs::copy(&path, &target).with_context(|| {
                    format!("copying {} → {}", path.display(), target.display())
                })?;
            }
        }
        Ok(())
    }

    /// Drive the `docker` stage when [`StageId::Docker`] is in the
    /// requested stage set.
    ///
    /// Delegates to [`anodizer_core::docker_build::oci_build_fixture`]
    /// (the allow-listed subprocess entry point), which runs
    /// `docker buildx build --output=type=oci,rewrite-timestamp=true,dest=…`
    /// against a staged build context (see [`stage_docker_context`]) whose
    /// layout mirrors what the production `docker` stage produces:
    /// `<context>/<os>/<arch>/<bin>` plus a `<context>/Dockerfile` copy.
    /// The repo `Dockerfile` does
    /// `COPY ${TARGETOS}/${TARGETARCH}/${BIN} …`, so building against the
    /// bare worktree (which has no `<os>/<arch>/<bin>` tree) fails the
    /// `COPY`; the harness must replicate the real stage's staging step.
    /// The emitted OCI tarball is copied into `<worktree>/dist/docker/` so
    /// the existing [`discover_artifacts`] walker picks it up under the
    /// normal `dist/` surface.
    ///
    /// Skipped (Ok no-op) when `<worktree>/Dockerfile` does not exist —
    /// the harness must stay harmless for repos whose docker config
    /// points at a non-default path (and for the cargo-package /
    /// cargo-only test fixtures that share the same harness binary).
    /// This skip is unconditional: a missing Dockerfile yields nothing to
    /// byte-compare, so it is not coverage loss regardless of intent.
    ///
    /// When `docker buildx` is unreachable or the project opted into
    /// `use: podman`, the behaviour forks on `explicitly_requested`:
    /// - `true` (operator typed `--stages=…,docker`): a hard ERROR. A
    ///   determinism gate that silently skips a stage the caller asked it
    ///   to byte-verify is false coverage — a non-reproducible image could
    ///   ship while the gate reports green. The release pipeline's ubuntu
    ///   shard requests docker explicitly and provisions a
    ///   `docker-container` buildx driver, so this error fires only when
    ///   that provisioning regressed.
    /// - `false` (auto-included): a warning through the harness logger (so
    ///   `-q` silences it). The harness also runs on minimal images (e.g.
    ///   the docs build container) that legitimately lack Docker; failing
    ///   the whole harness there would block unrelated stages. `docker` is
    ///   never in the default or `installers` stage sets today, so this
    ///   branch is reserved for any future auto-inclusion path.
    fn run_docker_stage(
        &self,
        worktree_path: &Path,
        env: &HashMap<String, String>,
        explicitly_requested: bool,
    ) -> Result<()> {
        let dockerfile = worktree_path.join("Dockerfile");
        if !dockerfile.exists() {
            return Ok(());
        }
        // The determinism harness's docker probe shells out to
        // `docker buildx build --output=type=oci,rewrite-timestamp=true,...`.
        // Those BuildKit-only flags are not recognised by plain
        // `podman build`; when the project config opts into `use: podman`
        // the only honest behaviour is to skip the docker stage with a
        // clear message, rather than spawn `docker buildx` and hand the
        // operator a misleading "this image is reproducible" signal that
        // covers a binary they will never actually publish.
        // These warnings go through the harness logger so `-q` governs
        // them like every other harness line; the docker-buildx child's
        // own output below is captured by `run_checked` and surfaced only
        // at `-v` (or on failure), so it honours the same verbosity flag.
        let log = StageLogger::new("check-determinism", self.verbosity);
        if self.docker_backend_hint.as_deref() == Some("podman") {
            let msg = "docker stage requested but project config has `use: podman` \
                 (Linux-only); the determinism harness only probes BuildKit-based \
                 builds. Verify podman image byte-stability outside the harness.";
            if explicitly_requested {
                anyhow::bail!(
                    "{msg} Refusing to report byte-stability for an image the harness \
                     cannot probe — remove `docker` from --stages or build the image \
                     with BuildKit."
                );
            }
            log.warn(&format!("{msg} The docker stage is skipped for this run."));
            return Ok(());
        }
        match anodizer_core::docker_detect::buildx_available() {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                if explicitly_requested {
                    anyhow::bail!(
                        "docker stage requested via --stages but `docker buildx` is not \
                         available on PATH; the determinism gate cannot byte-verify the \
                         image. Provision a `docker-container` buildx driver \
                         (docker/setup-buildx-action) before running the harness."
                    );
                }
                log.warn(
                    "skipped docker stage for this run — `docker buildx` is not available on PATH \
                     (no artifacts emitted)",
                );
                return Ok(());
            }
        }
        // The repo Dockerfile does `COPY ${TARGETOS}/${TARGETARCH}/${BIN}`,
        // so the build context must hold each binary at `<os>/<arch>/<bin>`.
        // Stage a dedicated context that mirrors the production `docker`
        // stage's layout (`stage-docker`'s `stage_artifacts_v2`) before
        // building against it; the bare worktree has no such tree.
        let context_dir = worktree_path.join(".det-tmp").join("docker-context");
        let staged = stage_docker_context(worktree_path, &context_dir, &dockerfile, &log)?;
        if staged == 0 {
            // No per-triple binaries discovered means the build pipeline
            // produced nothing the Dockerfile's COPY could resolve. Honour
            // the explicit-vs-auto fork rather than spawn a build that is
            // guaranteed to fail the COPY with a cryptic BuildKit error.
            if explicitly_requested {
                anyhow::bail!(
                    "docker stage requested via --stages but the build produced no \
                     per-triple binaries to stage under <os>/<arch>/; the COPY in \
                     `Dockerfile` cannot resolve. Check that the requested --targets \
                     built successfully."
                );
            }
            log.warn(
                "skipped docker stage for this run — no per-triple binaries to stage \
                 under <os>/<arch>/ (no artifacts emitted)",
            );
            return Ok(());
        }
        // Pin the image tag to a deterministic constant so the manifest's
        // `org.opencontainers.image.ref.name` annotation does not itself
        // drift between runs based on time-derived names.
        let output = anodizer_core::docker_build::oci_build_fixture(
            &context_dir,
            "anodize/det:harness",
            env,
            &log,
        )?;
        let dest_dir = worktree_path.join("dist").join("docker");
        std::fs::create_dir_all(&dest_dir)
            .with_context(|| format!("creating dest dir {}", dest_dir.display()))?;
        // Rename to a stable filename so the artifact-discovery walker
        // surfaces a single canonical row regardless of where buildx
        // emitted the tarball under the worktree.
        let target = dest_dir.join("image.oci.tar");
        std::fs::copy(&output.oci_tar_path, &target).with_context(|| {
            format!(
                "copying {} → {}",
                output.oci_tar_path.display(),
                target.display()
            )
        })?;
        // Capture the BuildKit-reported image digest alongside the OCI
        // tarball so the report records it as a separately-diffed
        // artifact. The two are independent stability signals: the
        // tarball hash covers serialized bytes (layer tar member
        // ordering, manifest serialization), while the iidfile records
        // BuildKit's pre-serialization manifest digest. Both must be
        // stable for the image to be declared byte-stable.
        if let Some(digest) = output.image_digest.as_deref() {
            std::fs::write(dest_dir.join("image.digest"), digest).with_context(|| {
                format!(
                    "writing image digest to {}",
                    dest_dir.join("image.digest").display()
                )
            })?;
        }
        Ok(())
    }

    /// Aggregate per-run hashes into the final report.
    fn build_report(
        &self,
        per_run_hashes: Vec<BTreeMap<String, ArtifactInfo>>,
    ) -> DeterminismReport {
        // Union of artifact names across runs — an artifact missing from
        // one run is itself a form of drift, surfaced as the run's hash
        // becoming `<missing>`.
        let mut all_names: BTreeSet<String> = BTreeSet::new();
        for run in &per_run_hashes {
            for name in run.keys() {
                all_names.insert(name.clone());
            }
        }

        let mut artifacts: Vec<ArtifactRow> = Vec::new();
        let mut drift: Vec<DriftRow> = Vec::new();
        let mut drift_count: u32 = 0;

        for name in &all_names {
            let mut hashes: Vec<String> = Vec::with_capacity(per_run_hashes.len());
            // Use the LAST run that produced the artifact as the source
            // of truth for path/size (matches "last writer wins"
            // semantics for the cosmetic fields).
            let mut last_info: Option<&ArtifactInfo> = None;
            for run in &per_run_hashes {
                match run.get(name) {
                    Some(info) => {
                        hashes.push(info.hash.clone());
                        last_info = Some(info);
                    }
                    None => hashes.push("<missing>".into()),
                }
            }

            let info = last_info.expect("artifact name came from union of run maps");
            let all_equal =
                hashes.iter().all(|h| h == &hashes[0]) && !hashes.iter().any(|h| h == "<missing>");
            // Sign-stage drift auto-allowlist: cosign sign-blob uses
            // ECDSA P-256 with a random nonce, so its signature bytes
            // can never be byte-identical across runs. Byte-equality is
            // not the right determinism signal for signatures —
            // verification (`cosign verify-blob` / `gpg --verify`) is.
            let signed_artifact_drift = !all_equal && info.stage == "sign";
            let allow_reason = self.resolve_allow_reason(name).or_else(|| {
                if signed_artifact_drift {
                    Some(
                        "signed artifact: signature bytes vary by signer \
                         (cosign ECDSA random nonce); validate via \
                         `cosign verify-blob` / `gpg --verify`"
                            .into(),
                    )
                } else {
                    None
                }
            });

            if all_equal {
                artifacts.push(ArtifactRow {
                    name: name.clone(),
                    path: info.relative_path.clone(),
                    size_bytes: info.size_bytes,
                    stage: info.stage.clone(),
                    deterministic: true,
                    nondeterministic_reason: allow_reason.clone(),
                    hash: Some(hashes[0].clone()),
                    hashes: vec![],
                });
            } else {
                artifacts.push(ArtifactRow {
                    name: name.clone(),
                    path: info.relative_path.clone(),
                    size_bytes: info.size_bytes,
                    stage: info.stage.clone(),
                    deterministic: false,
                    nondeterministic_reason: allow_reason.clone(),
                    hash: None,
                    hashes: hashes.clone(),
                });
                // Drift row + drift_count are gated on allow-list status:
                // allow-listed artifacts surface their per-run hashes via
                // the drift row (so the audit trail is complete) but DO
                // NOT bump `drift_count`.
                if allow_reason.is_none() {
                    let summary = summarize_drift(name, &per_run_hashes);
                    drift.push(DriftRow {
                        artifact: name.clone(),
                        hashes,
                        differing_bytes_summary: summary,
                    });
                    drift_count += 1;
                }
            }
        }

        DeterminismReport {
            schema_version: CURRENT_SCHEMA_VERSION,
            anodize_version: env!("CARGO_PKG_VERSION").into(),
            commit: self.commit.clone(),
            commit_timestamp: self.sde,
            runs: self.runs,
            stages_under_test: self.stages.iter().map(|s| s.as_str().into()).collect(),
            allowlist: self.allowlist.clone(),
            artifacts,
            drift,
            drift_count,
        }
    }

    /// Match `artifact_name` against the harness allow-list. Compile-time
    /// entries win on collision.
    fn resolve_allow_reason(&self, artifact_name: &str) -> Option<String> {
        for entry in &self.allowlist.compile_time {
            if matches_artifact_pattern(&entry.artifact, artifact_name) {
                return Some(entry.reason.clone());
            }
        }
        for entry in &self.allowlist.runtime {
            if matches_artifact_pattern(&entry.artifact, artifact_name) {
                return Some(entry.reason.clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::artifacts::{HEAD_SAMPLE_BYTES, TAIL_SAMPLE_BYTES, infer_stage_from_path};
    use super::*;
    use anodizer_core::AllowListEntry;

    fn empty_harness() -> Harness {
        Harness {
            repo_root: PathBuf::from("/tmp/unused"),
            commit: "deadbeef".into(),
            stages: vec![StageId::Archive, StageId::Checksum],
            runs: 2,
            sde: 1_715_000_000,
            allowlist: AllowList::default(),
            report_path: PathBuf::from("/tmp/unused/report.json"),
            inject_drift: None,
            targets: None,
            preserve_dist: None,
            version_hint: String::new(),
            child_snapshot: true,
            docker_backend_hint: None,
            crate_name: None,
            verbosity: Verbosity::Normal,
        }
    }

    fn run_with_files(
        h: &Harness,
        runs: Vec<Vec<(&str, &[u8])>>,
    ) -> Vec<BTreeMap<String, ArtifactInfo>> {
        // Synthesize per-run hash maps as if the child build pipeline
        // had emitted each file. Bypasses the actual subprocess so unit
        // tests don't depend on cargo / rustup / git.
        let _ = h;
        runs.into_iter()
            .map(|files| {
                let mut map = BTreeMap::new();
                for (name, bytes) in files {
                    use sha2::{Digest, Sha256};
                    let mut hasher = Sha256::new();
                    hasher.update(bytes);
                    let digest = format!("sha256:{:x}", hasher.finalize());
                    let head_len = bytes.len().min(HEAD_SAMPLE_BYTES);
                    let tail_sample = if bytes.len() > HEAD_SAMPLE_BYTES + TAIL_SAMPLE_BYTES {
                        bytes[bytes.len() - TAIL_SAMPLE_BYTES..].to_vec()
                    } else {
                        Vec::new()
                    };
                    map.insert(
                        name.into(),
                        ArtifactInfo {
                            hash: digest,
                            size_bytes: bytes.len() as u64,
                            relative_path: format!("dist/{}", name),
                            stage: infer_stage_from_path(name),
                            head_sample: bytes[..head_len].to_vec(),
                            tail_sample,
                        },
                    );
                }
                map
            })
            .collect()
    }

    #[test]
    fn harness_report_shape_serializes_correctly() {
        let h = empty_harness();
        let runs = run_with_files(
            &h,
            vec![
                vec![("anodizer_0.2.1.tar.gz", b"hello")],
                vec![("anodizer_0.2.1.tar.gz", b"hello")],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.runs, 2);
        assert_eq!(report.commit, "deadbeef");
        assert_eq!(report.stages_under_test, vec!["archive", "checksum"]);
        assert_eq!(report.drift_count, 0);
        assert_eq!(report.artifacts.len(), 1);
        assert!(report.artifacts[0].deterministic);
        assert!(report.artifacts[0].hash.is_some());
        assert!(report.artifacts[0].hashes.is_empty());

        // Round-trip JSON.
        let s = serde_json::to_string_pretty(&report).unwrap();
        let back: DeterminismReport = serde_json::from_str(&s).unwrap();
        assert_eq!(back, report);
    }

    #[test]
    fn harness_diffs_artifacts_by_sha256() {
        let h = empty_harness();
        let runs = run_with_files(
            &h,
            vec![
                vec![("stable.tar.gz", b"hello"), ("drifting.tar.gz", b"first")],
                vec![("stable.tar.gz", b"hello"), ("drifting.tar.gz", b"second")],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(report.drift_count, 1);
        assert_eq!(report.drift.len(), 1);
        assert_eq!(report.drift[0].artifact, "drifting.tar.gz");
        assert_eq!(report.drift[0].hashes.len(), 2);
        assert_ne!(report.drift[0].hashes[0], report.drift[0].hashes[1]);
        // Diagnostic: the drift row must carry a `differing_bytes_summary`
        // so future fix-cycles aren't blind.
        let summary = report.drift[0]
            .differing_bytes_summary
            .as_deref()
            .expect("drift row must populate differing_bytes_summary");
        assert!(
            summary.contains("offset 0x0"),
            "summary should point at byte 0 for diverging single-byte prefixes. got={summary}"
        );

        // Both artifacts appear in `artifacts`, with the stable one
        // marked deterministic and the drifting one marked not.
        let stable = report
            .artifacts
            .iter()
            .find(|a| a.name == "stable.tar.gz")
            .unwrap();
        let drifting = report
            .artifacts
            .iter()
            .find(|a| a.name == "drifting.tar.gz")
            .unwrap();
        assert!(stable.deterministic);
        assert!(!drifting.deterministic);
        assert!(drifting.hash.is_none());
        assert_eq!(drifting.hashes.len(), 2);
    }

    #[test]
    fn harness_excludes_allowlisted_artifacts_from_drift() {
        let mut h = empty_harness();
        // `.flatpak` is genuinely allow-listed (intrinsically non-reproducible
        // OSTree commit metadata); use it as the example so the fixture does
        // not model a now-gated format as non-deterministic.
        h.allowlist.compile_time.push(AllowListEntry {
            artifact: "*.flatpak".into(),
            reason: "flatpak build-bundle OSTree commit metadata not byte-stable".into(),
        });
        let runs = run_with_files(
            &h,
            vec![
                vec![("anodizer_0.2.1_linux_amd64.flatpak", b"flatpak-bytes-A")],
                vec![("anodizer_0.2.1_linux_amd64.flatpak", b"flatpak-bytes-B")],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(
            report.drift_count, 0,
            "allowlisted artifact must not bump drift_count"
        );
        let row = &report.artifacts[0];
        assert_eq!(row.name, "anodizer_0.2.1_linux_amd64.flatpak");
        assert!(!row.deterministic);
        assert_eq!(
            row.nondeterministic_reason.as_deref(),
            Some("flatpak build-bundle OSTree commit metadata not byte-stable")
        );
        assert_eq!(row.hashes.len(), 2);
    }

    #[test]
    fn harness_treats_missing_artifact_in_one_run_as_drift() {
        let h = empty_harness();
        let runs = run_with_files(&h, vec![vec![("only-in-run-1.tar.gz", b"present")], vec![]]);
        let report = h.build_report(runs);
        assert_eq!(report.drift_count, 1);
        assert_eq!(report.drift[0].artifact, "only-in-run-1.tar.gz");
        assert!(report.drift[0].hashes.iter().any(|h| h == "<missing>"));
    }

    #[test]
    fn matches_artifact_pattern_handles_glob_and_exact() {
        assert!(matches_artifact_pattern("*.crate", "foo.crate"));
        assert!(!matches_artifact_pattern("*.crate", "foo.tar.gz"));
        assert!(matches_artifact_pattern("exact.bin", "exact.bin"));
        assert!(!matches_artifact_pattern("exact.bin", "other.bin"));
    }

    #[test]
    fn stage_id_round_trips_to_string() {
        assert_eq!(StageId::Build.as_str(), "build");
        assert_eq!(StageId::Archive.as_str(), "archive");
        assert_eq!(StageId::Sbom.as_str(), "sbom");
        assert_eq!(StageId::Sign.as_str(), "sign");
        assert_eq!(StageId::Checksum.as_str(), "checksum");
    }

    /// Default `--stages=build,archive,sbom,sign,checksum` MUST drive
    /// `compute_extra_skip` to emit produce-stages like `nfpm`, `nsis`,
    /// `msi`, `dmg`, `pkg`, `snapcraft`, `source`, `flatpak`,
    /// `appbundle`, `srpm`, `upx`, `makeself`, `notarize`. Without this,
    /// the child release subprocess attempts e.g. `nfpm pkg --packager
    /// deb` on a macOS shard and dies with `No such file or directory`.
    #[test]
    fn harness_extra_skip_with_default_stages_includes_nfpm() {
        let stages = vec![
            StageId::Build,
            StageId::Archive,
            StageId::Sbom,
            StageId::Sign,
            StageId::Checksum,
        ];
        let extra = compute_extra_skip(&stages);
        for name in [
            "nfpm",
            "nsis",
            "msi",
            "dmg",
            "pkg",
            "snapcraft",
            "source",
            "flatpak",
            "appbundle",
            "srpm",
            "upx",
            "makeself",
            "notarize",
        ] {
            assert!(
                extra.iter().any(|s| s == name),
                "compute_extra_skip(default-stages) missing `{name}`: {extra:?}"
            );
        }
    }

    /// PRESERVE_SET stages MUST never appear in the extra skip list,
    /// regardless of whether the operator listed them via `--stages=`.
    /// Skipping `validate` would let bad configs through; skipping
    /// `before` would silently drop user hooks; skipping `templatefiles`
    /// would leave downstream stages without their materialized inputs.
    #[test]
    fn harness_extra_skip_omits_preserve_set() {
        let stages = vec![StageId::Build, StageId::Archive];
        let extra = compute_extra_skip(&stages);
        for name in PRESERVE_SET {
            assert!(
                !extra.iter().any(|s| s == name),
                "compute_extra_skip emitted PRESERVE_SET stage `{name}`: {extra:?}"
            );
        }
    }

    /// `changelog` is NOT in PRESERVE_SET — its output isn't a built
    /// artifact the harness diffs, `use=github-native` is inherently
    /// non-deterministic (depends on remote API state), and the harness
    /// env strips `GITHUB_TOKEN` for hermeticity so the stage would
    /// bail on tag-push runs. The publish-only path still runs the
    /// changelog stage with the real token, so the GitHub Release body
    /// is unaffected.
    #[test]
    fn harness_extra_skip_includes_changelog() {
        let stages = vec![StageId::Build, StageId::Archive];
        let extra = compute_extra_skip(&stages);
        assert!(
            extra.iter().any(|s| s == "changelog"),
            "compute_extra_skip missing `changelog`: {extra:?}"
        );
    }

    /// If the operator names a produce-stage in `--stages=`, the harness
    /// MUST NOT add it to the extra skip list — that would defeat the
    /// whole point of asking for it.
    #[test]
    fn harness_extra_skip_omits_requested_stages() {
        let stages = vec![StageId::Build, StageId::Archive, StageId::Sign];
        let extra = compute_extra_skip(&stages);
        for name in ["build", "archive", "sign"] {
            assert!(
                !extra.iter().any(|s| s == name),
                "compute_extra_skip dropped requested stage `{name}`: {extra:?}"
            );
        }
    }

    /// `SIDE_EFFECT_STAGES` entries are added back unconditionally by
    /// the runner's `compute_skip_arg`, so the harness's complement set
    /// shouldn't double-list them.
    #[test]
    fn harness_extra_skip_excludes_side_effect_stages() {
        use anodizer_core::determinism_runner::SIDE_EFFECT_STAGES;
        let stages = vec![StageId::Build];
        let extra = compute_extra_skip(&stages);
        for &name in SIDE_EFFECT_STAGES {
            assert!(
                !extra.iter().any(|s| s == name),
                "compute_extra_skip double-listed side-effect stage `{name}`: {extra:?}"
            );
        }
    }

    #[test]
    fn report_drift_count_matches_drift_array_len() {
        let h = empty_harness();
        let runs = run_with_files(
            &h,
            vec![
                vec![("a.tar.gz", b"x"), ("b.tar.gz", b"y"), ("c.tar.gz", b"z")],
                vec![
                    ("a.tar.gz", b"x"),
                    ("b.tar.gz", b"y-different"),
                    ("c.tar.gz", b"z-different"),
                ],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(report.drift.len() as u32, report.drift_count);
        assert_eq!(report.drift_count, 2);
    }

    /// A missing Dockerfile is an unconditional Ok no-op — there is
    /// nothing to byte-compare, so it is never coverage loss, even when
    /// the operator explicitly requested the docker stage.
    #[test]
    fn docker_stage_no_dockerfile_is_ok_even_when_explicit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let h = empty_harness();
        let env = HashMap::new();
        assert!(
            h.run_docker_stage(tmp.path(), &env, true).is_ok(),
            "missing Dockerfile must be a harmless no-op regardless of intent"
        );
    }

    /// The podman backend hint short-circuits before the buildx probe, so
    /// this exercises the explicit-vs-auto fork deterministically on any
    /// host (docker need not be installed).
    ///
    /// Explicitly-requested (`--stages=…,docker`): the harness must HARD
    /// ERROR rather than warn-and-skip. Silently skipping a stage the
    /// caller asked it to byte-verify is false coverage — a
    /// non-reproducible image could ship while the gate reports green.
    #[test]
    fn docker_stage_podman_explicit_request_is_hard_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        let mut h = empty_harness();
        h.docker_backend_hint = Some("podman".into());
        let env = HashMap::new();
        let err = h
            .run_docker_stage(tmp.path(), &env, true)
            .expect_err("explicit docker request under podman must fail the run, not skip");
        let msg = err.to_string();
        assert!(
            msg.contains("podman") && msg.contains("Refusing"),
            "error must explain the false-coverage refusal: {msg}"
        );
    }

    /// The docker staging step must lay each discovered per-triple binary
    /// out at `<os>/<arch>/<bin>` (matching the repo Dockerfile's
    /// `COPY ${TARGETOS}/${TARGETARCH}/${BIN}`) and drop the Dockerfile at
    /// the staging root, BEFORE any `docker buildx build` spawns. This
    /// exercises the staging logic in isolation — no docker required.
    #[test]
    fn docker_context_staging_lays_out_os_arch_bin_and_dockerfile() {
        let tmp = tempfile::TempDir::new().unwrap();
        let worktree = tmp.path();

        // Simulate the harness's discovered per-triple binaries: a linux
        // amd64 and an arm64 build under `.det-tmp/target/<triple>/release/`.
        for triple in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"] {
            let release = worktree
                .join(".det-tmp")
                .join("target")
                .join(triple)
                .join("release");
            std::fs::create_dir_all(&release).unwrap();
            std::fs::write(release.join("anodizer"), b"fake-binary").unwrap();
            // A bare host build + scratch dirs must be ignored by staging.
        }
        let host_release = worktree.join(".det-tmp").join("target").join("release");
        std::fs::create_dir_all(&host_release).unwrap();
        std::fs::write(host_release.join("anodizer"), b"host-byproduct").unwrap();

        let dockerfile = worktree.join("Dockerfile");
        std::fs::write(&dockerfile, "FROM scratch\nCOPY x x\n").unwrap();

        let context_dir = worktree.join(".det-tmp").join("docker-context");
        let log = StageLogger::new("test", Verbosity::Quiet);
        let staged = stage_docker_context(worktree, &context_dir, &dockerfile, &log).unwrap();

        // Both per-triple binaries staged; the bare host byproduct excluded.
        assert_eq!(staged, 2, "only per-triple binaries should be staged");
        assert!(
            context_dir
                .join("linux")
                .join("amd64")
                .join("anodizer")
                .is_file(),
            "amd64 binary must land at <context>/linux/amd64/anodizer"
        );
        assert!(
            context_dir
                .join("linux")
                .join("arm64")
                .join("anodizer")
                .is_file(),
            "arm64 binary must land at <context>/linux/arm64/anodizer"
        );
        assert!(
            context_dir.join("Dockerfile").is_file(),
            "Dockerfile must be copied to the staging root"
        );

        // Re-running wipes stale bytes: stale content must not survive.
        std::fs::write(context_dir.join("stale.txt"), b"old").unwrap();
        let staged2 = stage_docker_context(worktree, &context_dir, &dockerfile, &log).unwrap();
        assert_eq!(staged2, 2);
        assert!(
            !context_dir.join("stale.txt").exists(),
            "re-run must wipe the prior staging dir so no bytes carry over"
        );
    }

    /// Auto-included (not explicitly typed): the podman/buildx-absent
    /// path must remain a warn-and-skip so the harness stays harmless on
    /// minimal hosts. `docker` is never auto-included today, but the fork
    /// must preserve this branch for any future auto-inclusion path.
    #[test]
    fn docker_stage_podman_auto_included_warns_and_skips() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        let mut h = empty_harness();
        h.docker_backend_hint = Some("podman".into());
        let env = HashMap::new();
        assert!(
            h.run_docker_stage(tmp.path(), &env, false).is_ok(),
            "auto-included docker under podman must warn-and-skip, not error"
        );
    }
}
