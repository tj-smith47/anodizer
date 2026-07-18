//! Determinism Harness — drives N from-clean rebuilds in hermetic
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

mod aggregate;
mod artifacts;
mod docker;
mod drift;
mod env;
mod installer_detect;
mod preserve;
mod report;
mod run;
mod stage_id;

pub use installer_detect::installer_stages;
pub use stage_id::StageId;

use anodizer_core::determinism::AggregateKind;
use anodizer_core::git::worktree::Worktree;
use anodizer_core::harness_signing::EphemeralSigningKeys;
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::{AllowList, ArtifactRow, CURRENT_SCHEMA_VERSION, DeterminismReport, DriftRow};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use strum::{EnumIter, IntoEnumIterator};

use artifacts::{
    ArtifactInfo, copy_artifacts_to_dump, discover_artifacts, hash_artifacts,
    infer_stage_from_path, prune_dump_to_drifted,
};
use drift::{inject_drift_byte, pick_first_artifact_for_stage, summarize_drift};
use env::{BuildSubprocessEnv, build_subprocess_env};
use preserve::{
    ContextInputs, preserve_dist_tree, preserve_raw_binaries, remove_preserved_on_drift,
    write_preserved_dist_context,
};

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

/// Whether a stage consumes the compiled release binary as input, so the
/// child release pipeline MUST run the `build` stage to produce it.
///
/// `--stages=` is the harness's "what to diff" filter; it does not name
/// `build` when the operator only wants, say, `appimage,flatpak`. But every
/// binary-wrapping stage (AppImage, flatpak, nfpm, the OS installers, the
/// archive/upx/sign/checksum chain, docker) needs a compiled binary on disk.
/// If [`compute_extra_skip`] let the child skip `build` in that case, the run
/// either trips [`anodizer_core::binary_artifact_guard`] (for guard-armed
/// surfaces like `flatpak`/`nfpm`) or silently produces nothing — a false
/// determinism pass. So `build` is force-retained whenever any requested stage
/// returns `true` here.
///
/// Exhaustive by construction: a new [`StageId`] forces a deliberate
/// classification here rather than defaulting into a silent skip.
fn stage_requires_binary(stage: StageId) -> bool {
    match stage {
        // Source-only stages — they archive / package the source tree and
        // need no compiled binary. (`cargo package` does its own build-less
        // packaging; the source RPM ships a spec + source tarball, compiled
        // later on the target.)
        StageId::Source | StageId::Srpm | StageId::CargoPackage => false,
        // The install-script derives its case tables from configured release
        // intent (not from produced binaries), so it needs no compiled binary.
        StageId::InstallScript => false,
        // `build` is itself the producer; when requested it is already kept
        // by `requested_names`, so its answer here is immaterial.
        StageId::Build => false,
        // Everything else wraps, packs, compresses, signs, checksums, or
        // images the compiled binary.
        StageId::Upx
        | StageId::Archive
        | StageId::Nfpm
        | StageId::Makeself
        | StageId::Snapcraft
        | StageId::Sbom
        | StageId::Sign
        | StageId::Checksum
        | StageId::Docker
        | StageId::Msi
        | StageId::Nsis
        | StageId::Dmg
        | StageId::Pkg
        | StageId::Appbundle
        | StageId::Appimage
        | StageId::Flatpak => true,
    }
}

/// Compute the harness's child-subprocess "extra skip" set — every stage
/// name in [`anodizer_core::context::VALID_RELEASE_SKIPS`] that is NOT:
///
/// - in the operator's requested-stages list (`requested`), OR
/// - `build` when any requested stage consumes a compiled binary
///   (see [`stage_requires_binary`]), OR
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
///
/// The `build` carve-out is what lets an EXPLICIT binary-consuming subset
/// (`--stages=appimage,flatpak`, `--stages=nfpm`, ...) work without the
/// operator also having to remember to type `build`: a required input the
/// harness can derive is never imposed as operator config.
fn compute_extra_skip(requested: &[StageId]) -> Vec<String> {
    use anodizer_core::context::VALID_RELEASE_SKIPS;
    use anodizer_core::determinism_runner::SIDE_EFFECT_STAGES;
    let requested_names: BTreeSet<&str> = requested.iter().map(|s| s.as_str()).collect();
    let force_build = requested.iter().copied().any(stage_requires_binary);
    VALID_RELEASE_SKIPS
        .iter()
        .copied()
        .filter(|name| !requested_names.contains(name))
        .filter(|name| !(force_build && *name == "build"))
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
/// stage's layout so a `docker buildx build` against it resolves both the
/// configured dockerfile's `COPY ${TARGETOS}/${TARGETARCH}/${BIN}` and any
/// `COPY <extra_file> …`.
///
/// Reference: `anodizer_stage_docker::prepare_v2_config` — it stages
/// per-artifact binaries at `<context>/<os>/<arch>/<name>`, copies the
/// rendered dockerfile to `<context>/Dockerfile` ([`anodizer_stage_docker::copy_dockerfile`]),
/// then stages `extra_files` ([`anodizer_stage_docker::stage_extra_files`]).
/// The harness reuses the SAME two helpers so the two paths cannot drift; it
/// only substitutes the binary source, staging from the per-triple binaries
/// it discovered on disk (the harness ran the release as a subprocess and has
/// no in-process `Context` artifact set), mapping each triple → `(os, arch)`
/// via the same [`anodizer_core::target::map_target`] the real stage uses.
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
    docker_cfg: &ResolvedDockerConfig,
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

    // Copy the configured, template-rendered dockerfile (NOT a hardcoded
    // repo-root `Dockerfile`). Passed absolute so the copy is cwd-independent.
    let dockerfile_abs = worktree_path.join(&docker_cfg.dockerfile);
    anodizer_stage_docker::copy_dockerfile(
        &dockerfile_abs.to_string_lossy(),
        context_dir,
        false,
        log,
        "determinism docker",
    )?;

    // Stage the entry's extra_files preserving their relative structure,
    // rooting relative sources at the per-run worktree (`base_dir`) so the copy
    // reads the COMMITTED bytes — never the harness's own cwd (the live working
    // tree), which would leak uncommitted bytes into a rebuild that must
    // reflect the committed commit. No process-global cwd mutation.
    if !docker_cfg.extra_files.is_empty() {
        anodizer_stage_docker::stage_extra_files(
            &docker_cfg.extra_files,
            context_dir,
            Some(worktree_path),
            false,
            log,
            "determinism docker",
        )?;
    }

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
            // Skip cargo's `.cargo-*-lock` dotfiles (no extension ⇒ the filter
            // below would mistake them for the binary; a binary is never a dotfile).
            if path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|n| n.starts_with('.'))
            {
                continue;
            }
            match path.extension().and_then(|s| s.to_str()) {
                None | Some("exe") => out.push((triple.to_string(), path)),
                _ => continue,
            }
        }
    }
    out.sort();
    Ok(out)
}

/// One project `dockers_v2` entry resolved for the harness docker path.
///
/// Template rendering happens once in the CLI dispatcher (which owns a
/// [`anodizer_core::context::Context`]); the harness receives plain data and
/// mirrors the production `docker` stage's staging — `copy_dockerfile` +
/// `stage_extra_files` from [`anodizer_stage_docker`] — so the determinism
/// probe builds the SAME image the release build does, never the repo-root
/// `Dockerfile`.
#[derive(Debug, Clone)]
pub struct ResolvedDockerConfig {
    /// Template-rendered dockerfile path, relative to the repo/worktree root
    /// (the configured `dockers_v2[*].dockerfile`, e.g.
    /// `Dockerfile.agent.release`). Joined against each per-run worktree so
    /// the committed dockerfile is built, not the live working tree's.
    pub dockerfile: String,
    /// Configured `dockers_v2[*].extra_files`, relative to the repo/worktree
    /// root. Staged into the build context preserving their relative
    /// structure so a Dockerfile `COPY <extra> …` resolves.
    pub extra_files: Vec<String>,
    /// Template-rendered `--build-arg KEY=VALUE` pairs
    /// (`dockers_v2[*].build_args`), forwarded to buildx exactly as the
    /// production `build_docker_v2_command` does.
    pub build_args: Vec<(String, String)>,
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
    /// Resolved stage set under test — the operator's `--stages=` subset
    /// when given, otherwise [`super::commands::check::determinism::default_stages_for_host`]'s
    /// OS-native partition. Surfaced into the report's `stages_under_test`
    /// field and drives what the child release subprocess builds.
    pub stages: Vec<StageId>,
    /// The stages the operator EXPLICITLY typed into `--stages=` (empty when
    /// the set was resolved from the host default). Distinct from [`Self::stages`]
    /// because tool-absence handling forks on operator intent: an explicitly
    /// requested installer/docker stage whose tool is missing is a HARD ERROR
    /// (a silent skip would be false coverage), whereas a host-default stage
    /// whose tool is absent warn-skips so the harness stays usable on hosts
    /// lacking that toolchain. Threaded into [`Harness::gate_installer_stages`]
    /// and the docker fork.
    pub explicit_stages: Vec<StageId>,
    /// CI strict-tools mode (`--require-tools`). When `true`, the tool gate
    /// treats the ENTIRE resolved stage set as hard-fail-on-missing — a
    /// host-default OS-native producer whose tool is absent fails the run
    /// instead of warn-skipping. CI sets this so a `default_stages_for_host`
    /// run (no `--stages`) cannot silently under-cover a release the way the
    /// removed per-shard `det_stages` naming used to guard against. When
    /// `false` (dev default), only operator-typed [`Self::explicit_stages`]
    /// hard-fail; host-default stages warn-skip so dev boxes stay usable.
    pub require_tools: bool,
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
    /// Resolved `dockers_v2` entries for the crate under test (empty when the
    /// crate configures none). Drives [`Harness::run_docker_stage`]: each
    /// entry's rendered dockerfile + `extra_files` + `build_args` reproduce
    /// exactly what the production `docker` stage would build. An empty vec is
    /// a clean docker-stage skip — the harness never falls back to a stray
    /// repo-root `Dockerfile`. When a project declares MULTIPLE entries for
    /// the crate, every configured image is byte-verified, not just one.
    pub docker_configs: Vec<ResolvedDockerConfig>,
    /// Whether the crate under test DECLARES a non-empty `dockers_v2` block in
    /// config — independent of render outcome. Distinguishes the two empty-
    /// [`Self::docker_configs`] cases so [`Harness::run_docker_stage`] never
    /// SILENTLY passes a declared-but-unbuilt image: `false` → the crate
    /// configures no docker image (clean, quiet skip); `true` with empty
    /// `docker_configs` → the crate declared images but every entry was
    /// LEGITIMATELY skipped in this context (truthy `skip:` / empty-rendered
    /// conditional dockerfile) → visible warn-skip that MIRRORS production
    /// (which builds nothing, no error). A resolution ERROR keeps this `false`:
    /// under an explicit request it hard-fails upstream via
    /// `resolve_docker_configs`'s `?` propagation and never reaches the harness;
    /// under a host-default run the dispatcher warns accurately and forces this
    /// `false` (reflecting the errored resolve as not-declared) so the
    /// empty-state path here only ever means genuine all-skipped, never a
    /// swallowed error. Warning (not bailing) on the legit-skip case avoids the
    /// false FAILURE of reddening every determinism run of a `skip-on-snapshot`
    /// config.
    pub docker_declared: bool,
    /// When set alongside `preserve_dist`, the preserved dist tree is
    /// written to `<preserve_dist>/<crate_name>/` rather than directly
    /// into `<preserve_dist>/`. This prevents context.json collision when
    /// multiple crates release in parallel and their dist trees are merged
    /// by `download-artifact merge-multiple: true`.
    ///
    /// When `preserve_dist` is `None`, this field has no effect.
    pub crate_name: Option<String>,
    /// External tool requirements that can only be known from the loaded
    /// config, keyed by the stage that needs them. Threaded into
    /// [`Harness::gate_installer_stages`] so the gate can hard-fail a
    /// host-default producer whose backing binary is absent — never a
    /// host-static guess that could drift from what the build actually spawns.
    ///
    /// Current members, both resolved once by the dispatcher from the loaded
    /// config (tests insert entries directly):
    /// - [`StageId::Msi`] → the WiX binaries the resolved version spawns,
    ///   via `anodizer_stage_msi::required_msi_tools` (v3 → `candle`+`light`,
    ///   v4 → `wix`, the Linux path → `wixl`). The canonical drift case is a
    ///   `version: v3` config (candle+light) that a hardcoded `wix` probe
    ///   would have wrongly skipped.
    /// - [`StageId::Upx`] → each enabled `upx:` entry's binary (default
    ///   `upx`), via `anodizer_stage_upx::required_upx_tools` (the same SSOT
    ///   release preflight consults), so a configured host-default upx run
    ///   hard-fails under `--require-tools` instead of warn-skipping.
    ///
    /// A stage absent from this map carries no config-resolved requirement and
    /// falls back to the host-static probe table (or passes through).
    pub config_tools: BTreeMap<StageId, Vec<String>>,
    /// Absolute free-space floor (bytes) the headroom guard requires before
    /// each determinism run starts. The SOLE gate before run-0 (no prior
    /// peak measured yet, so a liveness backstop, not a peak guarantee) and
    /// a backstop for run-1..N when their measured peak × factor is below
    /// it.
    ///
    /// Derived, not required: the dispatcher resolves it via
    /// [`anodizer_core::disk::abs_floor_bytes_from_env`] (the
    /// `ANODIZER_DET_DISK_FLOOR_GIB` override, else
    /// [`anodizer_core::disk::DEFAULT_ABS_FLOOR_BYTES`]). Tests inject a
    /// value directly to exercise the guard without a real low-disk host.
    pub disk_abs_floor_bytes: u64,
    /// Multiplier applied to a prior run's MEASURED peak consumption when
    /// gating run-1..N (slack above the observed peak for sampling jitter,
    /// not a net→peak amplification). Derived via
    /// [`anodizer_core::disk::safety_factor_from_env`]
    /// (`ANODIZER_DET_DISK_SAFETY_FACTOR` override, else
    /// [`anodizer_core::disk::DEFAULT_SAFETY_FACTOR`]).
    pub disk_safety_factor: f64,
}

/// Hard-fail before any worktree is created when this run is about to build
/// a windows-msvc target (explicitly via `targets`, or implicitly via a
/// windows-msvc host build with no explicit `--target`) and `clang-cl` is
/// not on PATH.
///
/// `clang-cl` is the deterministic C/C++ compiler pin the harness's child
/// env wires in for cc-rs/cmake-driven crates (see
/// [`env::build_subprocess_env`] /
/// `anodizer_core::determinism::msvc_c_toolchain_env`). Without this gate a
/// missing `clang-cl` surfaces deep inside the child build as a confusing
/// cc-rs "failed to find tool" spawn error, well after worktree setup and
/// registry prefetch have already run. `probe` is injected (not a bare
/// [`anodizer_core::tool_detect::on_path`] call) so the hard-fail wiring is
/// unit-testable without depending on whether clang-cl happens to be
/// installed on the test host — same shape as [`Harness::gate_installer_stages`].
fn require_c_toolchain<P>(targets: &[String], host_is_windows_msvc: bool, probe: P) -> Result<()>
where
    P: Fn(&str) -> bool,
{
    let needs_clang_cl = if targets.is_empty() {
        host_is_windows_msvc
    } else {
        targets
            .iter()
            .any(|t| anodizer_core::target::is_windows_msvc(t))
    };
    if needs_clang_cl && !probe("clang-cl") {
        anyhow::bail!(
            "windows-msvc determinism requires `clang-cl` (LLVM) on PATH for byte-reproducible \
             C objects (zstd-sys/ring/aws-lc-sys/…); install LLVM or add its bin dir to PATH"
        );
    }
    Ok(())
}

/// Category a produced dist file falls into under the exhaustive classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Classification {
    /// A registered aggregate (combined checksums / `artifacts.json`).
    Aggregate,
    /// A `.sha256` / `.sig` derivative of a tracked primary.
    Sidecar,
    /// A tracked build/stage output, allow-listed format, or manifest member.
    Primary,
    /// None of the above — a hard fail (could mask drift).
    Unclassified,
}

/// Verdict of the transitive-derivation rule on a drifting aggregate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AggregateVerdict {
    /// Every differing member is allow-listed; the carried string is the
    /// audit reason.
    Excused(String),
    /// These members drifted and are NOT allow-listed — a real regression.
    Regression(Vec<String>),
    /// The aggregate's members could not be reconstructed/attributed; the
    /// carried string explains why. Treated as real drift, never excused.
    FailClosed(String),
}

/// Strip a `.sha256` / `.sig` sidecar suffix, returning the covered stem.
fn strip_sidecar_suffix(name: &str) -> Option<&str> {
    name.strip_suffix(".sha256")
        .or_else(|| name.strip_suffix(".sig"))
}

/// Last path component of a `/`- or `\`-separated key.
fn basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

#[cfg(test)]
mod tests;
