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

mod artifacts;
mod drift;
mod env;
mod installer_detect;
mod preserve;

pub use installer_detect::installer_stages;

use anodizer_core::determinism::AggregateKind;
use anodizer_core::git::worktree::Worktree;
use anodizer_core::harness_signing::EphemeralSigningKeys;
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::{AllowList, ArtifactRow, CURRENT_SCHEMA_VERSION, DeterminismReport, DriftRow};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

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

/// Stage subset selector for `--stages=<subset>`.
///
/// Currently informational: every variant maps to "run the build-side
/// pipeline and look at the artifacts that stage produces". The harness
/// shells to `anodize release --snapshot --skip=...` which runs the full
/// build-side pipeline; finer-grained per-stage gating is a follow-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    /// AppImage reproducibility probe (Linux).
    ///
    /// Drives the `anodizer_stage_appimage` crate. Primary gating tool is
    /// `linuxdeploy`; skipped at the harness gate when it is not on `PATH`
    /// so the harness stays usable on hosts (and CI shards) without the
    /// AppImage toolchain installed.
    Appimage,
    /// Flatpak reproducibility probe (Linux).
    ///
    /// Drives the `anodizer_stage_flatpak` crate. Primary gating tool is
    /// `flatpak-builder`; skipped at the harness gate when it is not on
    /// `PATH` so the harness stays usable on hosts (and CI shards) without
    /// the Flatpak toolchain installed.
    Flatpak,
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
            StageId::Appimage => "appimage",
            StageId::Flatpak => "flatpak",
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

impl Harness {
    /// Apply the external-tool availability gate to `effective_stages`,
    /// returning the stages whose backing tool is reachable.
    ///
    /// Covers every tool-gated producer: the installer family (`wix`,
    /// `rpmbuild`, `makensis`, …), the Linux package formats (`appimage`,
    /// `flatpak`), and the config-resolved stages (`msi`'s WiX version,
    /// `upx`'s binary — see [`Self::config_tools`]). The pipeline would
    /// otherwise fail mid-run at `Command::new("wix")` /
    /// `Command::new("rpmbuild")`, or — for `upx` — silently warn-skip the
    /// stage at runtime, surfacing a confusing error or false coverage
    /// instead of an honest "tool absent". A stage the operator EXPLICITLY
    /// typed into `--stages` (tracked in [`Self::explicit_stages`]) whose
    /// tool is missing is a HARD ERROR (a silent skip would be false
    /// determinism coverage). A host-default stage (resolved into
    /// [`Self::stages`] but never typed) whose tool is missing — e.g.
    /// `appimage` without `linuxdeploy` on the Linux default — warns and
    /// drops the stage so the harness stays usable. Stages with no tool
    /// requirement pass through.
    ///
    /// Under [`Self::require_tools`] (CI's `--require-tools`) the hard-fail
    /// contract widens to the ENTIRE resolved set: a host-default OS-native
    /// producer with a missing tool fails the run too, closing the silent-
    /// under-coverage hole that the removed per-shard `det_stages` naming
    /// used to guard.
    ///
    /// `probe` is injected so the hard-fail wiring is unit-testable
    /// without depending on which tools the host has installed.
    fn gate_installer_stages<P>(
        &self,
        effective_stages: &[StageId],
        probe: P,
    ) -> Result<Vec<StageId>>
    where
        P: Fn(&str) -> bool,
    {
        let gate = installer_detect::filter_available_with_probe(
            effective_stages,
            &self.config_tools,
            probe,
        );
        // The hard-fail set: under `--require-tools` (CI) the WHOLE resolved
        // stage set must have its tools present, so a host-default OS-native
        // producer with a missing tool fails the run. Otherwise only the
        // operator-typed explicit stages hard-fail; host-default stages warn-
        // skip below so dev boxes without the full toolchain stay usable.
        let hard_fail_set: &[StageId] = if self.require_tools {
            effective_stages
        } else {
            &self.explicit_stages
        };
        let hard_failed = gate.explicitly_skipped(hard_fail_set);
        if !hard_failed.is_empty() {
            anyhow::bail!(installer_detect::missing_tool_error(
                &hard_failed,
                self.require_tools
            ));
        }
        // Routed through the harness logger (not a bare eprintln) so
        // `-q` silences these like every other harness line. Only
        // non-hard-fail (host-default, no `--require-tools`) skips reach
        // here; a hard-fail set member already errored above.
        let warn_log = StageLogger::new("check-determinism", self.verbosity);
        for (stage, tool) in &gate.skipped {
            warn_log.warn(&format!(
                "skipped stage `{}` for this run — `{}` is not on PATH \
                 (no artifacts emitted)",
                stage.as_str(),
                tool
            ));
        }
        Ok(gate.available)
    }

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

        let effective_stages =
            self.gate_installer_stages(&effective_stages, installer_detect::host_tool_probe)?;

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

        // Shared, lock-pinned CARGO_HOME for the WHOLE invocation, hoisted
        // OUT of the per-run worktree (which the loop wipes each iteration).
        // This is the load-bearing change that makes every rebuild network-
        // free: the run-0 prefetch warms this registry cache once, and it
        // survives into runs 1..N instead of being re-downloaded from clean
        // each time. Determinism-safe to share — `.crate` tarballs + their
        // extracted sources are content-addressed and pinned by `Cargo.lock`,
        // so byte-identical no matter which run fetched them. Only COMPILED
        // output must stay per-run-fresh, and it does: `CARGO_TARGET_DIR`
        // lives inside the worktree and is wiped with it every iteration.
        let shared_cargo_home =
            worktree_root.join(format!("anodize-determinism-cargo-{}", std::process::id()));
        std::fs::create_dir_all(&shared_cargo_home)?;

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

        // Largest MEASURED peak consumption observed across prior runs, the
        // bound the headroom guard projects forward for run-1..N. `None`
        // until run-0's sampler reports (and stays `None` if the probe is
        // unavailable on this host). The net-vs-peak distinction is the
        // whole point: a between-runs net delta misses the mid-dmg peak.
        let mut max_prior_peak: Option<u64> = None;
        // One-shot latch so a permanently-broken free-space probe warns
        // ONCE (loud enough to notice in CI history) and then degrades
        // quietly, rather than spamming a warn per run.
        let mut probe_gap_warned = false;

        for run_idx in 0..self.runs {
            log.detail(&format!("run {} of {}", run_idx + 1, self.runs));
            // Probe free space BEFORE this run touches disk and apply the
            // fail-fast headroom guard. run-1..N are gated on the largest
            // measured peak of any prior run (× safety factor); run-0 has
            // no prior peak and is gated by the absolute floor alone.
            // `worktree_root` is the parent of the per-run worktree, so it
            // backs the same volume — probe it (it exists; the per-run
            // `worktree_path` does not until `Worktree::add`).
            let free_before = anodizer_core::disk::available_bytes(&worktree_root);
            if free_before.is_none() && !probe_gap_warned {
                log.warn(&format!(
                    "free-space probe unavailable on {} — determinism disk-headroom guard \
                     disabled for this invocation (a permanently-failing probe would otherwise \
                     silently skip the guard for an entire CI history)",
                    worktree_root.display()
                ));
                probe_gap_warned = true;
            }
            self.guard_run_headroom(&log, run_idx, &worktree_root, free_before, max_prior_peak)?;

            // Defensive: prior aborted runs may have left the dir behind;
            // `git worktree add` would reject a populated target.
            let _ = std::fs::remove_dir_all(&worktree_path);
            let worktree = Worktree::add(&self.repo_root, &worktree_path, &self.commit)
                .with_context(|| format!("creating worktree for determinism run {}", run_idx))?;
            if run_idx == 0 {
                // Warm the shared registry cache ONCE — online, with retries
                // — before any child build runs. Every rebuild below is
                // sealed offline (`CARGO_NET_OFFLINE=true` in the child env),
                // so this is the single, survivable network touch-point. The
                // man-page `before:` hook (`cargo run … man`) was merely the
                // FIRST cargo call to hit the empty per-run cache and flake on
                // a transient `Could not resolve host: index.crates.io`; a warm
                // shared cache plus the offline seal removes that live-crates.io
                // dependency from the gate entirely.
                log.verbose("prefetching dependencies into shared cargo home (online, retried)");
                anodizer_core::determinism_runner::prefetch_deps(
                    worktree.path(),
                    &shared_cargo_home,
                )
                .context("prefetching dependencies for the determinism harness")?;
            }
            let env =
                self.build_isolated_env(&worktree, &shared_cargo_home, signing_keys.as_ref())?;
            // Sample free space throughout the build + produce stages so the
            // mid-dmg PEAK (the actual ENOSPC moment) is measured, not the
            // post-reclaim net residue. On an error path the sampler's
            // `Drop` reaps the thread; we only read its minimum on success.
            let sampler = anodizer_core::disk::FreeSpaceSampler::start(
                &worktree_root,
                anodizer_core::disk::DEFAULT_SAMPLE_INTERVAL,
            );
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
                // Fork on operator INTENT, not mere set membership: the Linux
                // host default now includes `docker`, so `self.stages` holds
                // it on a bare run too. An explicitly typed `--stages=…,docker`
                // (tracked in `explicit_stages`) — or any docker under CI's
                // `--require-tools` — hard-fails when buildx is unreachable; a
                // plain host-default docker warn-skips so the harness stays
                // usable where Docker is absent. A gate that silently skips a
                // required stage is false coverage, hence the hard-error
                // contract for that case below.
                let docker_explicitly_requested =
                    self.require_tools || self.explicit_stages.contains(&StageId::Docker);
                self.run_docker_stage(worktree.path(), &env, docker_explicitly_requested)
                    .with_context(|| {
                        format!("running docker stage for determinism run {}", run_idx)
                    })?;
            }
            // Stop the sampler now the disk high-water mark has passed. The
            // peak = free-before − min-free-observed; fold it into
            // `max_prior_peak` so run-(idx+1)'s guard is gated on the
            // largest real peak seen so far. Emitted at verbose so the
            // first CI run surfaces run-0's true number (B1.3 / W1).
            let min_free_during = sampler.stop();
            if let (Some(before), Some(min_free)) = (free_before, min_free_during) {
                let peak = anodizer_core::disk::RunPeak {
                    free_before: before,
                    min_free_during: min_free,
                };
                let consumed = peak.consumed_bytes();
                let dist_size = anodizer_core::disk::dir_size_bytes(&worktree.path().join("dist"));
                log.verbose(&format!(
                    "disk peak run {}: consumed {} (min free {}, worktree dist {})",
                    run_idx + 1,
                    anodizer_core::disk::format_gib(consumed),
                    anodizer_core::disk::format_gib(min_free),
                    anodizer_core::disk::format_gib(dist_size),
                ));
                max_prior_peak = Some(max_prior_peak.map_or(consumed, |m| m.max(consumed)));
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
            // Inter-run reclamation: explicitly drop the worktree NOW
            // (rather than at the `}` below) so its entire tree —
            // `.det-tmp/target/**` (the per-run CARGO_TARGET_DIR, the
            // heavy scratch), `.det-tmp/home`, `dist/**`, and the raw
            // per-triple binaries — is freed by `Worktree::drop`'s
            // `git worktree remove --force` BEFORE the next iteration's
            // free-space probe and headroom guard run. Everything the
            // next run consumes is rebuilt from the detached commit, so
            // none of it is read across runs.
            //
            // Determinism-safe: by this point run-0's hashes are already
            // recorded (`per_run_hashes.push` above), the drift-bins dump
            // is already copied out, and — when `--preserve-dist` is set —
            // run-0's dist tree AND raw binaries are already mirrored to
            // `dest`. Nothing freed here feeds the byte comparison or the
            // preserved dist; the worktree is pure rebuild scratch. The
            // drift-bins dump under `<report>/drift-bins/run-N` is
            // deliberately NOT freed here — it is the drift diagnostic and
            // is pruned post-loop only after the comparison decides which
            // runs drifted.
            drop(worktree);
            if let Some(after) = anodizer_core::disk::available_bytes(&worktree_root) {
                log.verbose(&format!(
                    "disk free {}: {} after run {} (worktree reclaimed)",
                    worktree_root.display(),
                    anodizer_core::disk::format_gib(after),
                    run_idx + 1
                ));
            }
        }

        // Best-effort reclaim of the shared CARGO_HOME. It lives OUTSIDE the
        // worktree, so the per-run `Worktree::drop` never touches it; remove it
        // now that all runs are done. It's a throwaway cache (the next
        // invocation re-prefetches into its own pid-suffixed dir), so a leftover
        // on the error path is harmless.
        let _ = std::fs::remove_dir_all(&shared_cargo_home);

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

    /// Emit a verbose disk-headroom line for the worktree volume and apply
    /// the fail-fast guard before a determinism run starts.
    ///
    /// `vol` is the worktree-root path (its parent volume backs the
    /// per-run worktree); `free` is the available bytes already probed on
    /// it. `prior_peak` is the largest MEASURED peak consumption of any
    /// prior run (`None` before run-0, when only the absolute floor gates;
    /// `None` thereafter only if the probe was unavailable).
    ///
    /// Routine readings go to `verbose` (per the log-status-vs-verbose
    /// rule); a shortfall is the one default-visible disk event — surfaced
    /// as an `error` line and then returned as an `Err` that aborts the
    /// harness BEFORE the opaque `hdiutil` ENOSPC can fire. Probe gaps
    /// (`free == None`) degrade to a no-op: the guard never manufactures a
    /// failure from missing data (the one-shot warn at the call site
    /// records that the guard is disabled for the invocation).
    fn guard_run_headroom(
        &self,
        log: &StageLogger,
        run_idx: u32,
        vol: &Path,
        free: Option<u64>,
        prior_peak: Option<u64>,
    ) -> Result<()> {
        use anodizer_core::disk::{HeadroomDecision, evaluate_headroom, format_gib};
        let Some(free) = free else {
            return Ok(());
        };
        let vols = anodizer_core::disk::mounted_volumes();
        let mounts = if vols.is_empty() {
            String::new()
        } else {
            format!(" — /Volumes: [{}]", vols.join(", "))
        };
        log.verbose(&format!(
            "disk free {}: {} before run {}{}",
            vol.display(),
            format_gib(free),
            run_idx + 1,
            mounts
        ));
        match evaluate_headroom(
            run_idx,
            free,
            self.disk_abs_floor_bytes,
            prior_peak,
            self.disk_safety_factor,
            &vol.display().to_string(),
        ) {
            HeadroomDecision::Proceed => Ok(()),
            HeadroomDecision::Abort(shortfall) => {
                let msg = shortfall.message();
                log.error(&msg);
                anyhow::bail!(msg)
            }
        }
    }

    /// Construct the env map handed to each child build process.
    fn build_isolated_env(
        &self,
        worktree: &Worktree,
        cargo_home: &Path,
        signing_keys: Option<&EphemeralSigningKeys>,
    ) -> Result<HashMap<String, String>> {
        let tmpdir = worktree.path().join(".det-tmp");
        std::fs::create_dir_all(&tmpdir)?;
        // `cargo_home` is the invocation-wide shared cache (created once in
        // `run()` and warmed by the run-0 prefetch); only the compiled-output
        // dir is per-run-fresh inside the worktree.
        let cargo_target = tmpdir.join("target");
        let home_dir = tmpdir.join("home");
        std::fs::create_dir_all(&home_dir)?;

        Ok(build_subprocess_env(&BuildSubprocessEnv {
            cargo_home,
            cargo_target: &cargo_target,
            tmpdir: &tmpdir,
            home_dir: &home_dir,
            sde: self.sde,
            worktree: worktree.path(),
            targets: self.targets.as_deref().unwrap_or(&[]),
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
    /// - `false` (host-default, not operator-typed): a warning through the
    ///   harness logger (so `-q` silences it). `docker` IS in the Linux host
    ///   default, so a bare `anodize check determinism` on a Linux box without
    ///   `docker buildx` reaches this branch and warn-skips rather than failing
    ///   the whole harness — the harness also runs on minimal images (e.g. the
    ///   docs build container) that legitimately lack Docker, where failing
    ///   would block unrelated stages.
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

        // Authoritative produced-artifact set, parsed from the run's
        // `artifacts.json` manifest. Any dist file whose basename appears
        // here is a tracked primary — this covers template / extra /
        // uploadable files whose extension `infer_stage_from_path` cannot
        // classify (e.g. `install.sh`).
        let manifest_members = self.produced_member_basenames(&per_run_hashes);
        // Basenames the manifest flags as combined checksums files via the
        // `combined = "true"` marker — the authoritative aggregate signal,
        // independent of the operator's chosen filename (e.g. `SHA512SUMS`).
        let combined_markers = self.produced_combined_markers(&per_run_hashes);

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

            // Byte-equality is the determinism verdict; classification only
            // excuses a DRIFTING aggregate (below). An unclassified file fails
            // only when its bytes drift — a stable one cannot mask member
            // drift: every member is independently hashed and surfaces its own
            // drift row regardless of any aggregate that contains it.
            let classification =
                self.classify(name, &all_names, &manifest_members, &combined_markers);
            if matches!(classification, Classification::Unclassified) {
                artifacts.push(ArtifactRow {
                    name: name.clone(),
                    path: info.relative_path.clone(),
                    size_bytes: info.size_bytes,
                    stage: info.stage.clone(),
                    deterministic: all_equal,
                    nondeterministic_reason: None,
                    hash: if all_equal {
                        Some(hashes[0].clone())
                    } else {
                        None
                    },
                    hashes: if all_equal { vec![] } else { hashes.clone() },
                });
                if !all_equal {
                    drift.push(DriftRow {
                        artifact: name.clone(),
                        hashes,
                        differing_bytes_summary: Some(
                            "unclassified produced file drifted across runs; if it is a \
                             combined checksums file, mark it combined=true so its members \
                             can be evaluated — otherwise it is a real regression"
                                .into(),
                        ),
                    });
                    drift_count += 1;
                }
                continue;
            }

            // Transitive-derivation rule: a drifting aggregate is excused IFF
            // every differing member is itself allow-listed. An unexcused
            // member is a real regression; an aggregate whose members cannot
            // be reconstructed fails closed (never excused).
            let mut aggregate_excuse: Option<String> = None;
            if !all_equal && matches!(classification, Classification::Aggregate) {
                let kind = self
                    .aggregate_kind_for_name(name, &combined_markers)
                    .expect("Aggregate classification ⇒ a registered kind matches");
                match self.evaluate_aggregate(
                    kind.as_ref(),
                    name,
                    &per_run_hashes,
                    &combined_markers,
                ) {
                    AggregateVerdict::Excused(reason) => aggregate_excuse = Some(reason),
                    AggregateVerdict::Regression(members) => {
                        artifacts.push(ArtifactRow {
                            name: name.clone(),
                            path: info.relative_path.clone(),
                            size_bytes: info.size_bytes,
                            stage: info.stage.clone(),
                            deterministic: false,
                            nondeterministic_reason: None,
                            hash: None,
                            hashes: hashes.clone(),
                        });
                        // One drift row per aggregate (keeps the report's
                        // `drift_count == drift.len()` invariant); the
                        // offending members are named in both the artifact
                        // field and the summary.
                        let joined = members.join(", ");
                        drift.push(DriftRow {
                            artifact: format!("{name} → {joined}"),
                            hashes,
                            differing_bytes_summary: Some(format!(
                                "aggregate member(s) [{joined}] drifted and are not allow-listed; \
                                 a gated artifact regressed (surfaced via the {name} aggregate)"
                            )),
                        });
                        drift_count += 1;
                        continue;
                    }
                    AggregateVerdict::FailClosed(reason) => {
                        artifacts.push(ArtifactRow {
                            name: name.clone(),
                            path: info.relative_path.clone(),
                            size_bytes: info.size_bytes,
                            stage: info.stage.clone(),
                            deterministic: false,
                            nondeterministic_reason: None,
                            hash: None,
                            hashes: hashes.clone(),
                        });
                        drift.push(DriftRow {
                            artifact: name.clone(),
                            hashes,
                            differing_bytes_summary: Some(reason),
                        });
                        drift_count += 1;
                        continue;
                    }
                }
            }

            // Sign-stage drift auto-allowlist: cosign sign-blob uses
            // ECDSA P-256 with a random nonce, so its signature bytes
            // can never be byte-identical across runs. Byte-equality is
            // not the right determinism signal for signatures —
            // verification (`cosign verify-blob` / `gpg --verify`) is.
            let signed_artifact_drift = !all_equal && info.stage == "sign";
            let allow_reason = aggregate_excuse
                .or_else(|| self.resolve_allow_reason(name))
                .or_else(|| {
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

    /// Parse the run's `artifacts.json` manifest(s) into the set of produced
    /// member basenames. This is the authoritative "what did we produce"
    /// list, so any dist file whose basename appears here is a tracked
    /// primary regardless of its extension (covers `template_files` /
    /// `extra_files` / uploadable files). Reads the LAST run's manifest;
    /// the path set is identical across runs (only member digests drift).
    /// Best-effort: an absent or unparseable manifest yields an empty set
    /// (callers fall back to extension- and allow-list-based classification).
    fn produced_member_basenames(
        &self,
        per_run_hashes: &[BTreeMap<String, ArtifactInfo>],
    ) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let Some(run) = per_run_hashes.last() else {
            return out;
        };
        for (name, info) in run {
            if !anodizer_core::determinism::ArtifactsManifest.matches(name) {
                continue;
            }
            let Some(full) = info.full.as_deref() else {
                continue;
            };
            if let Ok(units) = anodizer_core::determinism::ArtifactsManifest.members_by_unit(full) {
                out.extend(units.into_values());
            }
        }
        out
    }

    /// Parse the run's `artifacts.json` manifest(s) into the set of basenames
    /// flagged as combined checksums files via the `combined = "true"` marker.
    /// This is the authoritative recognizer for the combined-checksums
    /// aggregate — it catches an operator-renamed file (`SHA512SUMS`) that the
    /// filename-suffix heuristic cannot. Best-effort: an absent / unparseable
    /// manifest yields an empty set (callers fall back to the suffix match).
    fn produced_combined_markers(
        &self,
        per_run_hashes: &[BTreeMap<String, ArtifactInfo>],
    ) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let Some(run) = per_run_hashes.last() else {
            return out;
        };
        for (name, info) in run {
            if !anodizer_core::determinism::ArtifactsManifest.matches(name) {
                continue;
            }
            let Some(full) = info.full.as_deref() else {
                continue;
            };
            if let Ok(markers) =
                anodizer_core::determinism::combined_checksum_members_from_manifest(full)
            {
                out.extend(markers);
            }
        }
        out
    }

    /// Resolve the [`AggregateKind`] for `name`, consulting the manifest's
    /// `combined = "true"` markers as well as the filename-suffix registry.
    /// The marker path lets an operator-renamed combined file (`SHA512SUMS`)
    /// be recognized as a [`CombinedChecksums`] aggregate.
    fn aggregate_kind_for_name(
        &self,
        name: &str,
        combined_markers: &BTreeSet<String>,
    ) -> Option<Box<dyn anodizer_core::determinism::AggregateKind>> {
        if let Some(kind) = anodizer_core::determinism::aggregate_kind_for(name) {
            return Some(kind);
        }
        if combined_markers.contains(basename(name)) {
            return Some(Box::new(anodizer_core::determinism::CombinedChecksums));
        }
        None
    }

    /// Classify a produced dist file. Order matters: a registered aggregate
    /// is recognized before the sidecar / primary checks so a combined
    /// `checksums.txt` is never mislabeled a plain checksum primary.
    fn classify(
        &self,
        name: &str,
        all_names: &BTreeSet<String>,
        manifest_members: &BTreeSet<String>,
        combined_markers: &BTreeSet<String>,
    ) -> Classification {
        if self
            .aggregate_kind_for_name(name, combined_markers)
            .is_some()
        {
            return Classification::Aggregate;
        }
        // Sidecar: a `.sha256` / `.sig` whose stripped stem names a primary.
        if let Some(stem) = strip_sidecar_suffix(name) {
            let stem_is_primary = self.is_primary(stem, manifest_members)
                || all_names
                    .iter()
                    .any(|n| n.as_str() == stem || basename(n) == basename(stem));
            if stem_is_primary {
                return Classification::Sidecar;
            }
        }
        if self.is_primary(name, manifest_members) {
            return Classification::Primary;
        }
        Classification::Unclassified
    }

    /// A *primary* artifact: a recognized build/stage output (known
    /// `infer_stage_from_path` attribution), an intrinsically
    /// non-deterministic allow-listed format, the explicitly-tracked
    /// `metadata.json`, or any file the run's manifest declares it produced.
    fn is_primary(&self, name: &str, manifest_members: &BTreeSet<String>) -> bool {
        let base = basename(name);
        infer_stage_from_path(name) != "unknown"
            || self.resolve_allow_reason(name).is_some()
            // `metadata.json` is a tracked primary (expected byte-stable);
            // its pass is explicit, not incidental.
            || base == anodizer_core::dist::METADATA_JSON
            || manifest_members.contains(base)
    }

    /// Apply the transitive-derivation rule to a drifting aggregate.
    ///
    /// Reconstructs each run's members from the aggregate's full bytes and
    /// computes the set of *differing* members (a unit absent from any run —
    /// added, removed, or value-changed). The aggregate is excused IFF every
    /// differing member is itself allow-listed; any unexcused member is a
    /// real regression. Fails closed when the bytes are missing / uncaptured
    /// / unparseable, or when bytes drifted yet no member unit changed
    /// (structural drift we cannot attribute).
    fn evaluate_aggregate(
        &self,
        kind: &dyn anodizer_core::determinism::AggregateKind,
        name: &str,
        per_run_hashes: &[BTreeMap<String, ArtifactInfo>],
        combined_markers: &BTreeSet<String>,
    ) -> AggregateVerdict {
        let mut visited: BTreeSet<String> = BTreeSet::new();
        visited.insert(name.to_string());
        self.evaluate_aggregate_inner(kind, name, per_run_hashes, combined_markers, &mut visited)
    }

    /// Recursive core of [`Self::evaluate_aggregate`]. `visited` tracks the
    /// aggregate names already on the evaluation stack so a nested aggregate
    /// that (pathologically) lists itself fails closed instead of recursing
    /// forever.
    fn evaluate_aggregate_inner(
        &self,
        kind: &dyn anodizer_core::determinism::AggregateKind,
        name: &str,
        per_run_hashes: &[BTreeMap<String, ArtifactInfo>],
        combined_markers: &BTreeSet<String>,
        visited: &mut BTreeSet<String>,
    ) -> AggregateVerdict {
        let mut maps: Vec<BTreeMap<String, String>> = Vec::with_capacity(per_run_hashes.len());
        for run in per_run_hashes {
            let Some(info) = run.get(name) else {
                return AggregateVerdict::FailClosed(format!(
                    "aggregate {name} missing from a run — cannot reconstruct members; \
                     treated as real drift"
                ));
            };
            let Some(full) = info.full.as_deref() else {
                return AggregateVerdict::FailClosed(format!(
                    "aggregate {name} full bytes not captured — cannot reconstruct members; \
                     treated as real drift"
                ));
            };
            match kind.members_by_unit(full) {
                Ok(m) => maps.push(m),
                Err(e) => {
                    return AggregateVerdict::FailClosed(format!(
                        "aggregate {name} failed to parse ({e:#}); treated as real drift"
                    ));
                }
            }
        }
        let n = maps.len();
        let mut all_keys: BTreeSet<&String> = BTreeSet::new();
        for m in &maps {
            all_keys.extend(m.keys());
        }
        let mut differing_members: BTreeSet<String> = BTreeSet::new();
        for key in all_keys {
            let present = maps.iter().filter(|m| m.contains_key(key)).count();
            if present < n
                && let Some(member) = maps.iter().find_map(|m| m.get(key))
            {
                differing_members.insert(member.clone());
            }
        }
        if differing_members.is_empty() {
            return AggregateVerdict::FailClosed(format!(
                "aggregate {name} bytes drifted but no member unit changed \
                 (structural / ordering drift); treated as real drift"
            ));
        }
        let mut unexcused: Vec<String> = Vec::new();
        for member in &differing_members {
            match self.member_excused(member, per_run_hashes, combined_markers, visited) {
                Ok(true) => {}
                Ok(false) => unexcused.push(member.clone()),
                Err(reason) => return AggregateVerdict::FailClosed(reason),
            }
        }
        if unexcused.is_empty() {
            AggregateVerdict::Excused(format!(
                "aggregate of derived rows: every differing member ({}) is allow-listed \
                 non-deterministic; each member is drift-checked independently",
                differing_members
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        } else {
            AggregateVerdict::Regression(unexcused)
        }
    }

    /// Whether a *differing* aggregate member is excused.
    ///
    /// A member is excused when it is directly allow-listed, OR when it is
    /// itself a (nested) aggregate whose own members all resolve as excused —
    /// the transitive rule applied recursively (`artifacts.json` ⊃
    /// `checksums.txt` ⊃ per-artifact rows). `Err` is fail-closed: the nested
    /// aggregate's bytes are missing / unparseable, or a membership cycle was
    /// hit — the caller treats it as real drift, never an excuse.
    fn member_excused(
        &self,
        member: &str,
        per_run_hashes: &[BTreeMap<String, ArtifactInfo>],
        combined_markers: &BTreeSet<String>,
        visited: &mut BTreeSet<String>,
    ) -> Result<bool, String> {
        if self.resolve_allow_reason(member).is_some() {
            return Ok(true);
        }
        let Some(kind) = self.aggregate_kind_for_name(member, combined_markers) else {
            return Ok(false);
        };
        // Resolve the basename `member` back to the actual artifact key so we
        // can fetch its full bytes (member came from a parent aggregate's
        // member map, where it is recorded as a bare basename).
        let agg_name = per_run_hashes
            .last()
            .and_then(|run| run.keys().find(|k| basename(k) == member).cloned());
        let Some(agg_name) = agg_name else {
            return Err(format!(
                "nested aggregate member {member} could not be located among produced \
                 artifacts — cannot verify its members; treated as real drift"
            ));
        };
        if !visited.insert(agg_name.clone()) {
            return Err(format!(
                "aggregate membership cycle detected at {agg_name}; treated as real drift"
            ));
        }
        let verdict = self.evaluate_aggregate_inner(
            kind.as_ref(),
            &agg_name,
            per_run_hashes,
            combined_markers,
            visited,
        );
        match verdict {
            AggregateVerdict::Excused(_) => Ok(true),
            AggregateVerdict::Regression(_) => Ok(false),
            AggregateVerdict::FailClosed(reason) => Err(reason),
        }
    }
}

/// Category a produced dist file falls into under the exhaustive classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Classification {
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
enum AggregateVerdict {
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
mod tests {
    use super::artifacts::{
        HEAD_SAMPLE_BYTES, TAIL_SAMPLE_BYTES, infer_stage_from_path, should_capture_full,
    };
    use super::*;
    use anodizer_core::AllowListEntry;

    fn empty_harness() -> Harness {
        Harness {
            repo_root: PathBuf::from("/tmp/unused"),
            commit: "deadbeef".into(),
            stages: vec![StageId::Archive, StageId::Checksum],
            explicit_stages: vec![StageId::Archive, StageId::Checksum],
            require_tools: false,
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
            config_tools: BTreeMap::new(),
            disk_abs_floor_bytes: anodizer_core::disk::DEFAULT_ABS_FLOOR_BYTES,
            disk_safety_factor: anodizer_core::disk::DEFAULT_SAFETY_FACTOR,
        }
    }

    /// A harness whose compile-time allow-list excuses the given glob
    /// patterns (e.g. `*.deb`) — the intrinsically-non-deterministic members
    /// the transitive-derivation rule should excuse.
    fn harness_with_allow(patterns: &[&str]) -> Harness {
        let mut h = empty_harness();
        h.allowlist = AllowList {
            compile_time: patterns
                .iter()
                .map(|p| AllowListEntry {
                    artifact: (*p).to_string(),
                    reason: format!("test: {p} is intrinsically non-deterministic"),
                })
                .collect(),
            runtime: Vec::new(),
        };
        h
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
                    // Mirror production's full-byte retention so the
                    // transitive-derivation rule (incl. marker-renamed combined
                    // files) can reconstruct members.
                    let full = if should_capture_full(name, bytes) {
                        Some(bytes.to_vec())
                    } else {
                        None
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
                            full,
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

    // --- Transitive-derivation rule for aggregate artifacts --------------

    /// No false positive: a combined checksums file drifts solely because an
    /// allow-listed member (`*.deb`, signed) changed its line. Every
    /// differing member is allow-listed ⇒ the aggregate is excused ⇒
    /// `drift_count == 0`.
    #[test]
    fn aggregate_excused_when_only_allowlisted_member_drifts() {
        let h = harness_with_allow(&["*.deb"]);
        let run0 = b"hashA  bar.tar.gz\ndeb000  app_1.0_amd64.deb\n" as &[u8];
        let run1 = b"hashA  bar.tar.gz\ndeb111  app_1.0_amd64.deb\n" as &[u8];
        let runs = run_with_files(
            &h,
            vec![
                vec![("app_checksums.txt", run0), ("bar.tar.gz", b"stable")],
                vec![("app_checksums.txt", run1), ("bar.tar.gz", b"stable")],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(
            report.drift_count, 0,
            "aggregate drift caused only by an allow-listed member must not fail"
        );
        let agg = report
            .artifacts
            .iter()
            .find(|a| a.name == "app_checksums.txt")
            .expect("checksums row present");
        assert!(!agg.deterministic);
        assert!(
            agg.nondeterministic_reason
                .as_deref()
                .is_some_and(|r| r.contains("app_1.0_amd64.deb")),
            "excuse must name the differing allow-listed member: {:?}",
            agg.nondeterministic_reason
        );
    }

    /// No masking: a GATED (supposedly byte-reproducible) member's line
    /// changed in the aggregate. Even with NO separate row for that member
    /// (only the aggregate is emitted), the aggregate must FAIL and name the
    /// offending member.
    #[test]
    fn aggregate_fails_when_gated_member_drifts_even_if_member_row_suppressed() {
        let h = harness_with_allow(&["*.deb"]);
        // Only the checksums file is emitted — the gated `bar.tar.gz` member
        // has no independent row, so the aggregate is the sole signal.
        let run0 = b"t000  bar.tar.gz\ndeb000  app_1.0_amd64.deb\n" as &[u8];
        let run1 = b"t111  bar.tar.gz\ndeb111  app_1.0_amd64.deb\n" as &[u8];
        let runs = run_with_files(
            &h,
            vec![
                vec![("app_checksums.txt", run0)],
                vec![("app_checksums.txt", run1)],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(
            report.drift_count, 1,
            "a gated member drifting inside the aggregate must surface as drift"
        );
        assert!(
            report
                .drift
                .iter()
                .any(|d| d.artifact.contains("bar.tar.gz")),
            "the offending gated member must be named: {:?}",
            report.drift
        );
        // The allow-listed deb that ALSO changed must NOT be reported.
        assert!(
            !report.drift.iter().any(|d| d.artifact.contains(".deb")),
            "allow-listed member must not be reported as a regression"
        );
    }

    /// A member appearing (added) is judged by its own allow-list status: a
    /// new GATED member fails; a removed ALLOW-LISTED member is excused.
    #[test]
    fn aggregate_judges_additions_and_removals_by_member_status() {
        // Addition of a gated member ⇒ fail.
        let h = harness_with_allow(&["*.deb"]);
        let add0 = b"a000  a.tar.gz\ndeb000  x_1.0_amd64.deb\n" as &[u8];
        let add1 = b"a000  a.tar.gz\ndeb000  x_1.0_amd64.deb\nb000  b.tar.gz\n" as &[u8];
        let runs = run_with_files(
            &h,
            vec![
                vec![("c_checksums.txt", add0)],
                vec![("c_checksums.txt", add1)],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(report.drift_count, 1, "added gated member must fail");
        assert!(report.drift.iter().any(|d| d.artifact.contains("b.tar.gz")));

        // Removal of an allow-listed member ⇒ excused.
        let rem0 = b"a000  a.tar.gz\ndeb000  x_1.0_amd64.deb\n" as &[u8];
        let rem1 = b"a000  a.tar.gz\n" as &[u8];
        let runs = run_with_files(
            &h,
            vec![
                vec![("c_checksums.txt", rem0)],
                vec![("c_checksums.txt", rem1)],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(
            report.drift_count, 0,
            "removing an allow-listed member must be excused"
        );
    }

    /// Fail-closed: an aggregate that drifts but cannot be parsed (or whose
    /// drift is structural, with no member unit changing) is treated as real
    /// drift, never excused.
    #[test]
    fn aggregate_fails_closed_on_unparseable_or_structural_drift() {
        let h = harness_with_allow(&["*.deb"]);
        // Structural drift: identical member set, only line ORDER changed.
        let s0 = b"a  a.tar.gz\nd  x_1.0_amd64.deb\n" as &[u8];
        // Reordered but same lines ⇒ parsed unit sets are identical ⇒ no
        // member differs ⇒ fail closed (cannot attribute the byte drift).
        let s1 = b"d  x_1.0_amd64.deb\na  a.tar.gz\nz  z\n" as &[u8];
        let runs = run_with_files(
            &h,
            vec![vec![("s_checksums.txt", s0)], vec![("s_checksums.txt", s1)]],
        );
        let report = h.build_report(runs);
        assert_eq!(
            report.drift_count, 1,
            "an aggregate whose drift cannot be attributed must fail closed"
        );
    }

    /// The artifacts.json manifest aggregate is judged member-by-member: a
    /// gated archive whose recorded digest changed fails; the same change to
    /// an allow-listed deb is excused.
    #[test]
    fn artifacts_manifest_transitive_rule() {
        let h = harness_with_allow(&["*.deb"]);
        let gated0 = br#"[
          {"kind":"archive","path":"./dist/a.tar.gz","name":"a.tar.gz","metadata":{"sha256":"aaaa"}},
          {"kind":"linux_package","path":"./dist/a_1.0_amd64.deb","name":"a_1.0_amd64.deb","metadata":{"sha256":"dddd"}}
        ]"# as &[u8];
        // The gated archive's recorded digest changed ⇒ regression.
        let gated1 = br#"[
          {"kind":"archive","path":"./dist/a.tar.gz","name":"a.tar.gz","metadata":{"sha256":"bbbb"}},
          {"kind":"linux_package","path":"./dist/a_1.0_amd64.deb","name":"a_1.0_amd64.deb","metadata":{"sha256":"dddd"}}
        ]"#;
        let runs = run_with_files(
            &h,
            vec![
                vec![("artifacts.json", gated0)],
                vec![("artifacts.json", gated1)],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(
            report.drift_count, 1,
            "gated archive digest drift must fail"
        );
        assert!(report.drift.iter().any(|d| d.artifact.contains("a.tar.gz")));

        // Only the allow-listed deb digest changed ⇒ excused.
        let deb1 = br#"[
          {"kind":"archive","path":"./dist/a.tar.gz","name":"a.tar.gz","metadata":{"sha256":"aaaa"}},
          {"kind":"linux_package","path":"./dist/a_1.0_amd64.deb","name":"a_1.0_amd64.deb","metadata":{"sha256":"eeee"}}
        ]"#;
        let runs = run_with_files(
            &h,
            vec![
                vec![("artifacts.json", gated0)],
                vec![("artifacts.json", deb1)],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(
            report.drift_count, 0,
            "deb-only digest drift must be excused"
        );
    }

    /// Finding 1: the combined-checksums aggregate is recognized by the
    /// `combined = "true"` manifest marker, not the filename suffix. An
    /// operator-renamed `SHA512SUMS` (which the suffix heuristic misses) is
    /// still subject to the transitive-derivation rule: excused when only an
    /// allow-listed member line drifts, failed when a gated member line drifts.
    #[test]
    fn marker_named_combined_file_obeys_transitive_rule() {
        let h = harness_with_allow(&["*.deb"]);
        // Manifest flags `SHA512SUMS` as combined; identical across runs so the
        // manifest itself doesn't drift — only the SHA512SUMS file does.
        let manifest = br#"[
          {"kind":"archive","path":"./dist/bar.tar.gz","name":"bar.tar.gz","metadata":{"sha256":"barbar"}},
          {"kind":"linux_package","path":"./dist/app_1.0_amd64.deb","name":"app_1.0_amd64.deb","metadata":{"sha256":"debdeb"}},
          {"kind":"checksum","path":"./dist/SHA512SUMS","name":"SHA512SUMS","metadata":{"combined":"true"}}
        ]"# as &[u8];
        // The suffix heuristic alone does NOT recognize this file.
        assert!(anodizer_core::determinism::aggregate_kind_for("SHA512SUMS").is_none());

        // Excused: only the allow-listed deb line drifts.
        let sums0 = b"barbar  bar.tar.gz\ndeb000  app_1.0_amd64.deb\n" as &[u8];
        let sums1 = b"barbar  bar.tar.gz\ndeb111  app_1.0_amd64.deb\n" as &[u8];
        let runs = run_with_files(
            &h,
            vec![
                vec![
                    ("artifacts.json", manifest),
                    ("SHA512SUMS", sums0),
                    ("bar.tar.gz", b"stable"),
                ],
                vec![
                    ("artifacts.json", manifest),
                    ("SHA512SUMS", sums1),
                    ("bar.tar.gz", b"stable"),
                ],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(
            report.drift_count, 0,
            "marker-named combined file drift from an allow-listed member must be excused: {:?}",
            report.drift
        );
        let agg = report
            .artifacts
            .iter()
            .find(|a| a.name == "SHA512SUMS")
            .expect("SHA512SUMS classified as an aggregate row");
        assert!(!agg.deterministic);
        assert!(
            agg.nondeterministic_reason
                .as_deref()
                .is_some_and(|r| r.contains("app_1.0_amd64.deb")),
            "excuse must name the drifting allow-listed member: {:?}",
            agg.nondeterministic_reason
        );

        // Fail: the gated archive line drifts inside the same renamed file.
        let g0 = b"bar000  bar.tar.gz\ndeb000  app_1.0_amd64.deb\n" as &[u8];
        let g1 = b"bar111  bar.tar.gz\ndeb000  app_1.0_amd64.deb\n" as &[u8];
        let runs = run_with_files(
            &h,
            vec![
                vec![
                    ("artifacts.json", manifest),
                    ("SHA512SUMS", g0),
                    ("bar.tar.gz", b"stable"),
                ],
                vec![
                    ("artifacts.json", manifest),
                    ("SHA512SUMS", g1),
                    ("bar.tar.gz", b"stable"),
                ],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(
            report.drift_count, 1,
            "a gated member drifting inside the renamed combined file must fail"
        );
        assert!(
            report
                .drift
                .iter()
                .any(|d| d.artifact.contains("bar.tar.gz")),
            "the gated member must be named: {:?}",
            report.drift
        );
    }

    /// Finding 2 (realistic permutation): a cfgd-style recut where the archive
    /// is byte-stable (gated) but the SBOM, cosign bundle, and detached
    /// signature drift. Their drift is excused (compile-time `*.cdx.json` +
    /// runtime `*.cosign.bundle` / `*.sig`), so `artifacts.json` — whose
    /// recorded digests for those members moved — is excused too. Flipping the
    /// gated archive then proves a real regression still surfaces.
    #[test]
    fn artifacts_manifest_recut_excuses_only_nondeterministic_members() {
        let mut h = empty_harness();
        h.allowlist = AllowList {
            compile_time: vec![
                AllowListEntry {
                    artifact: "*.cdx.json".into(),
                    reason: "CycloneDX SBOM carries a random serial UUID".into(),
                },
                AllowListEntry {
                    artifact: "*.deb".into(),
                    reason: "GPG-signed nfpm deb".into(),
                },
            ],
            runtime: vec![
                AllowListEntry {
                    artifact: "*.cosign.bundle".into(),
                    reason: "cosign ECDSA random nonce".into(),
                },
                AllowListEntry {
                    artifact: "*.sig".into(),
                    reason: "cosign detached signature".into(),
                },
            ],
        };
        let manifest = |arch: &str, sbom: &str, bundle: &str, sig: &str| {
            format!(
                r#"[
  {{"kind":"archive","path":"./dist/app.tar.gz","name":"app.tar.gz","metadata":{{"sha256":"{arch}"}}}},
  {{"kind":"sbom","path":"./dist/app.cdx.json","name":"app.cdx.json","metadata":{{"sha256":"{sbom}"}}}},
  {{"kind":"signature","path":"./dist/app.tar.gz.cosign.bundle","name":"app.tar.gz.cosign.bundle","metadata":{{"sha256":"{bundle}"}}}},
  {{"kind":"signature","path":"./dist/app.tar.gz.sig","name":"app.tar.gz.sig","metadata":{{"sha256":"{sig}"}}}},
  {{"kind":"checksum","path":"./dist/checksums.txt","name":"checksums.txt","metadata":{{"combined":"true"}}}},
  {{"kind":"metadata","path":"./dist/metadata.json","name":"metadata.json","metadata":{{}}}},
  {{"kind":"uploadable_file","path":"./dist/install.sh","name":"install.sh","metadata":{{"sha256":"inst"}}}}
]"#
            )
            .into_bytes()
        };
        let checksums = b"arch  app.tar.gz\n" as &[u8];
        let files = |m: &[u8], sbom: &[u8], bundle: &[u8], sig: &[u8]| -> Vec<(String, Vec<u8>)> {
            vec![
                ("artifacts.json".into(), m.to_vec()),
                ("app.tar.gz".into(), b"archive-stable".to_vec()),
                ("app.cdx.json".into(), sbom.to_vec()),
                ("app.tar.gz.cosign.bundle".into(), bundle.to_vec()),
                ("app.tar.gz.sig".into(), sig.to_vec()),
                ("checksums.txt".into(), checksums.to_vec()),
                ("metadata.json".into(), b"meta-stable".to_vec()),
                ("install.sh".into(), b"#!/bin/sh\n".to_vec()),
            ]
        };
        fn borrow(v: &[(String, Vec<u8>)]) -> Vec<(&str, &[u8])> {
            v.iter().map(|(n, b)| (n.as_str(), b.as_slice())).collect()
        }

        // Recut: archive stable, SBOM + bundle + sig all drift.
        let r0 = files(
            &manifest("ARCH", "SB0", "BUN0", "SIG0"),
            b"sbom-0",
            b"bundle-0",
            b"sig-0",
        );
        let r1 = files(
            &manifest("ARCH", "SB1", "BUN1", "SIG1"),
            b"sbom-1",
            b"bundle-1",
            b"sig-1",
        );
        let runs = run_with_files(&h, vec![borrow(&r0), borrow(&r1)]);
        let report = h.build_report(runs);
        assert_eq!(
            report.drift_count, 0,
            "a recut that only moves SBOM/cosign-bundle/sig must be fully excused: {:?}",
            report.drift
        );

        // Regression: the gated archive itself drifts (file + recorded digest).
        let g0 = files(
            &manifest("ARCH0", "SB0", "BUN0", "SIG0"),
            b"sbom-0",
            b"bundle-0",
            b"sig-0",
        );
        let mut g1 = files(
            &manifest("ARCH1", "SB0", "BUN0", "SIG0"),
            b"sbom-0",
            b"bundle-0",
            b"sig-0",
        );
        // Flip the archive bytes too so the file-level row also drifts.
        for entry in &mut g1 {
            if entry.0 == "app.tar.gz" {
                entry.1 = b"archive-DRIFTED".to_vec();
            }
        }
        let runs = run_with_files(&h, vec![borrow(&g0), borrow(&g1)]);
        let report = h.build_report(runs);
        assert!(
            report.drift_count >= 1,
            "a gated archive regression must surface"
        );
        assert!(
            report
                .drift
                .iter()
                .any(|d| d.artifact.contains("app.tar.gz")),
            "the gated archive must be named in drift: {:?}",
            report.drift
        );
    }

    /// Finding 2 (nested recursion): `artifacts.json` lists `checksums.txt` as
    /// a combined member; the inner `checksums.txt` drifts. The transitive rule
    /// recurses — excused when the inner drift is an allow-listed member,
    /// failed when the inner drift is a gated member.
    #[test]
    fn nested_aggregate_recursion_judges_inner_members() {
        let h = harness_with_allow(&["*.cdx.json"]);
        // Manifest records no digest for checksums.txt, so its content token is
        // the whole entry; bumping `size` makes the `checksums.txt` member of
        // artifacts.json drift, forcing the recursion path.
        let manifest = |size: u32| {
            format!(
                r#"[
  {{"kind":"archive","path":"./dist/app.tar.gz","name":"app.tar.gz","metadata":{{"sha256":"AAAA"}}}},
  {{"kind":"sbom","path":"./dist/app.cdx.json","name":"app.cdx.json","metadata":{{"sha256":"SB{size}"}}}},
  {{"kind":"checksum","path":"./dist/checksums.txt","name":"checksums.txt","metadata":{{"combined":"true"}},"size":{size}}}
]"#
            )
            .into_bytes()
        };

        // Excused: the inner checksums.txt drifts only at its allow-listed SBOM
        // line.
        let ck0 = b"arch  app.tar.gz\nsbom0  app.cdx.json\n" as &[u8];
        let ck1 = b"arch  app.tar.gz\nsbom1  app.cdx.json\n" as &[u8];
        let m0 = manifest(100);
        let m1 = manifest(101);
        let runs = run_with_files(
            &h,
            vec![
                vec![("artifacts.json", m0.as_slice()), ("checksums.txt", ck0)],
                vec![("artifacts.json", m1.as_slice()), ("checksums.txt", ck1)],
            ],
        );
        let report = h.build_report(runs);
        assert_eq!(
            report.drift_count, 0,
            "nested aggregate excused when inner drift is allow-listed: {:?}",
            report.drift
        );

        // Fail: the inner checksums.txt drifts at its GATED archive line.
        let bad0 = b"arch0  app.tar.gz\nsbom0  app.cdx.json\n" as &[u8];
        let bad1 = b"arch1  app.tar.gz\nsbom0  app.cdx.json\n" as &[u8];
        let runs = run_with_files(
            &h,
            vec![
                vec![("artifacts.json", m0.as_slice()), ("checksums.txt", bad0)],
                vec![("artifacts.json", m1.as_slice()), ("checksums.txt", bad1)],
            ],
        );
        let report = h.build_report(runs);
        assert!(
            report.drift_count >= 1,
            "nested aggregate must fail when an inner gated member drifts"
        );
        assert!(
            report
                .drift
                .iter()
                .any(|d| d.artifact.contains("checksums.txt") || d.artifact.contains("app.tar.gz")),
            "the failing nested member chain must be named: {:?}",
            report.drift
        );
    }

    /// Every file a normal run emits classifies (zero `Unclassified`), and a
    /// genuinely unregistered file is a hard fail even when byte-stable.
    #[test]
    fn unclassified_gates_on_byte_drift_not_on_classification() {
        let h = harness_with_allow(&["*.flatpak"]);
        // artifacts.json declares `install.sh` so it classifies as a tracked
        // primary via manifest membership (its `.sh` extension is unknown).
        let manifest = br#"[
          {"kind":"archive","path":"./dist/foo.tar.gz","name":"foo.tar.gz","metadata":{"sha256":"aaaa"}},
          {"kind":"uploadable_file","path":"./dist/install.sh","name":"install.sh","metadata":{"sha256":"bbbb"}},
          {"kind":"metadata","path":"./dist/metadata.json","name":"metadata.json","metadata":{}}
        ]"# as &[u8];
        let checksums = b"aaaa  foo.tar.gz\n" as &[u8];
        let files: Vec<(&str, &[u8])> = vec![
            ("foo.tar.gz", b"archive-bytes"),       // archive (infer)
            ("foo.tar.gz.sig", b"sig-bytes"),       // sidecar of a primary
            ("app_1.0_amd64.deb", b"deb-bytes"),    // nfpm (infer)
            ("app_1.0_amd64.flatpak", b"fp-bytes"), // allow-listed primary
            ("anodizer.1", b"man-bytes"),           // man page (infer)
            ("install.sh", b"sh-bytes"),            // manifest member primary
            ("metadata.json", b"meta-bytes"),       // explicit tracked primary
            ("app_checksums.txt", checksums),       // registered aggregate
            ("artifacts.json", manifest),           // registered aggregate
        ];
        let runs = run_with_files(&h, vec![files.clone(), files]);
        let report = h.build_report(runs);
        assert_eq!(
            report.drift_count, 0,
            "a fully-classified, byte-stable run must not fail. drift={:?}",
            report.drift
        );
        assert!(
            !report.drift.iter().any(|d| d
                .differing_bytes_summary
                .as_deref()
                .is_some_and(|s| s.contains("unclassified"))),
            "no file should be unclassified: {:?}",
            report.drift
        );

        // A genuinely unregistered file that is BYTE-STABLE passes: the gate
        // is byte-equality, not classification. A stable file cannot mask
        // member drift (identical aggregate bytes ⇒ identical members), so
        // there is nothing to fail.
        let stable: Vec<(&str, &[u8])> = vec![("mystery.xyz", b"same")];
        let report = h.build_report(run_with_files(&h, vec![stable.clone(), stable]));
        assert_eq!(
            report.drift_count, 0,
            "a byte-stable unclassified file must NOT fail: {:?}",
            report.drift
        );

        // The same unclassified file, now DRIFTING across runs, IS a hard
        // fail: no aggregate rule can excuse it, so it reads as a real
        // regression.
        let drifting: Vec<Vec<(&str, &[u8])>> =
            vec![vec![("mystery.xyz", b"one")], vec![("mystery.xyz", b"two")]];
        let report = h.build_report(run_with_files(&h, drifting));
        assert_eq!(
            report.drift_count, 1,
            "a drifting unclassified file must hard-fail: {:?}",
            report.drift
        );
        assert!(
            report.drift[0]
                .differing_bytes_summary
                .as_deref()
                .is_some_and(|s| s.contains("unclassified")),
            "the drift reason must flag the unclassified file: {:?}",
            report.drift
        );
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

    /// A minimal requested set (`build,archive,sbom,sign,checksum`) MUST
    /// drive `compute_extra_skip` to emit produce-stages like `nfpm`,
    /// `nsis`, `msi`, `dmg`, `pkg`, `snapcraft`, `source`, `flatpak`,
    /// `appbundle`, `srpm`, `upx`, `makeself`. Without this, the child
    /// release subprocess attempts e.g. `nfpm pkg --packager deb` on a
    /// macOS shard and dies with `No such file or directory`. `notarize`
    /// is NOT expected here — it is a `SIDE_EFFECT_STAGES` member, added
    /// to the child `--skip=` unconditionally by `compute_skip_arg`, so
    /// `compute_extra_skip` deliberately filters it out of the complement.
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
        ] {
            assert!(
                extra.iter().any(|s| s == name),
                "compute_extra_skip(default-stages) missing `{name}`: {extra:?}"
            );
        }
        // notarize is a side-effect stage now; compute_extra_skip must not
        // double-list it (compute_skip_arg adds it from SIDE_EFFECT_STAGES).
        assert!(
            !extra.iter().any(|s| s == "notarize"),
            "notarize must not appear in the complement set: {extra:?}"
        );
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

    /// An EXPLICIT binary-consuming subset (`--stages=appimage,flatpak`)
    /// MUST keep `build` enabled in the child pipeline even though the
    /// operator did not type `build`. Skipping it produces no binary, which
    /// trips the binary-artifact guard (flatpak is guard-armed) and aborts
    /// the run before either AppImage or flatpak is ever diffed.
    #[test]
    fn harness_extra_skip_retains_build_for_binary_consuming_subset() {
        for stages in [
            vec![StageId::Appimage, StageId::Flatpak],
            vec![StageId::Flatpak],
            vec![StageId::Nfpm],
            vec![StageId::Archive],
        ] {
            let extra = compute_extra_skip(&stages);
            assert!(
                !extra.iter().any(|s| s == "build"),
                "compute_extra_skip skipped `build` for binary-consuming subset {stages:?}: {extra:?}"
            );
        }
    }

    /// A source-only subset (`--stages=source`) needs no compiled binary,
    /// so `build` stays a normal skip candidate — the harness must not pay
    /// for a full release build it does not diff.
    #[test]
    fn harness_extra_skip_skips_build_for_source_only_subset() {
        for stages in [
            vec![StageId::Source],
            vec![StageId::CargoPackage],
            vec![StageId::Srpm],
        ] {
            let extra = compute_extra_skip(&stages);
            assert!(
                extra.iter().any(|s| s == "build"),
                "compute_extra_skip kept `build` for source-only subset {stages:?}: {extra:?}"
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

    /// An installer stage the operator explicitly typed into `--stages`
    /// whose tool is absent must HARD ERROR at the harness gate, mirroring
    /// the docker contract above. Silently warn-skipping a stage the
    /// caller asked it to byte-verify is false coverage — a
    /// non-reproducible installer could ship while the gate reports green.
    ///
    /// Drives the real [`Harness::gate_installer_stages`] (the smallest
    /// entry that invokes the gate `run()` itself calls) with an
    /// always-absent probe, so the assertion holds regardless of which
    /// installer tools the host has installed.
    #[test]
    fn installer_explicit_request_missing_tool_is_hard_error() {
        let mut h = empty_harness();
        h.stages = vec![StageId::Build, StageId::Nsis];
        // Operator typed these stages, so they enter the explicit set that the
        // hard-fail gate keys on.
        h.explicit_stages = h.stages.clone();
        let err = h
            .gate_installer_stages(&h.stages.clone(), |_tool| false)
            .expect_err(
                "explicit installer request with a missing tool must fail the run, not skip",
            );
        let msg = err.to_string();
        assert!(
            msg.contains("nsis") && msg.contains("makensis"),
            "error must name the missing stage and its tool: {msg}"
        );
        assert!(
            msg.contains("--stages"),
            "error must tell the operator how to opt out: {msg}"
        );
    }

    /// A non-explicit (auto-included) installer stage with a missing tool
    /// must warn-and-drop, not error, so the gate's available set still
    /// returns the non-installer stages. Pins the fork the hard-error
    /// test's sibling branch depends on.
    #[test]
    fn installer_non_explicit_missing_tool_warns_and_drops() {
        // `stages` (the operator's explicit set) holds only Build, so the
        // Nsis stage reaching the gate is treated as non-explicit.
        let h = empty_harness();
        let effective = vec![StageId::Build, StageId::Nsis];
        let available = h
            .gate_installer_stages(&effective, |_tool| false)
            .expect("non-explicit missing tool must warn-and-drop, not error");
        assert_eq!(
            available,
            vec![StageId::Build],
            "missing-tool installer must be dropped; non-installer stages pass through"
        );
    }

    /// Under CI's `--require-tools` the SAME host-default (non-explicit) stage
    /// that `installer_non_explicit_missing_tool_warns_and_drops` lets warn-skip
    /// must instead HARD-FAIL — closing the silent under-coverage hole left by
    /// removing the per-shard `det_stages` naming. `explicit_stages` stays empty
    /// (the operator typed nothing); only `require_tools` flips the contract.
    #[test]
    fn require_tools_hard_fails_host_default_missing_tool() {
        let mut h = empty_harness();
        h.explicit_stages = Vec::new();
        h.require_tools = true;
        let effective = vec![StageId::Build, StageId::Nsis];
        let err = h
            .gate_installer_stages(&effective, |_tool| false)
            .expect_err("--require-tools must hard-fail a host-default missing tool");
        let msg = err.to_string();
        assert!(
            msg.contains("nsis") && msg.contains("makensis"),
            "error must name the missing host-default stage and its tool: {msg}"
        );
    }

    /// `--require-tools` must NOT punish a host-default stage whose tool IS
    /// present — strict mode only fails on genuine absence, it does not force
    /// every OS-native producer to exist regardless.
    #[test]
    fn require_tools_keeps_host_default_when_tool_present() {
        let mut h = empty_harness();
        h.explicit_stages = Vec::new();
        h.require_tools = true;
        let effective = vec![StageId::Build, StageId::Nsis];
        let available = h
            .gate_installer_stages(&effective, |_tool| true)
            .expect("present tool must pass even under --require-tools");
        assert_eq!(available, vec![StageId::Build, StageId::Nsis]);
    }

    /// Release-blocker regression: `upx` is a host-default producer whose
    /// tool-presence was historically checked only by stage-upx's lenient
    /// runtime guard, which warn-skips even under `--require-tools`. With its
    /// resolved binary threaded into [`Harness::config_tools`], `--require-tools`
    /// must HARD-FAIL a host-default upx run whose binary is absent — naming
    /// the stage and the missing `upx` binary — instead of silently emitting
    /// no compressed artifact (false determinism coverage). `explicit_stages`
    /// stays empty (the operator typed nothing); only `require_tools` flips the
    /// contract, exactly as it does for the installer family.
    #[test]
    fn require_tools_hard_fails_host_default_missing_upx() {
        let mut h = empty_harness();
        h.explicit_stages = Vec::new();
        h.require_tools = true;
        h.config_tools.insert(StageId::Upx, vec!["upx".to_string()]);
        let effective = vec![StageId::Build, StageId::Upx];
        let err = h
            .gate_installer_stages(&effective, |_tool| false)
            .expect_err("--require-tools must hard-fail a host-default missing upx");
        let msg = err.to_string();
        assert!(
            msg.contains("upx"),
            "error must name the missing upx stage and its tool: {msg}"
        );
    }

    /// The flip side: `--require-tools` must NOT punish a host-default upx run
    /// whose binary IS present — the stage stays in the effective set and runs
    /// in the child release subprocess.
    #[test]
    fn require_tools_keeps_host_default_upx_when_tool_present() {
        let mut h = empty_harness();
        h.explicit_stages = Vec::new();
        h.require_tools = true;
        h.config_tools.insert(StageId::Upx, vec!["upx".to_string()]);
        let effective = vec![StageId::Build, StageId::Upx];
        let available = h
            .gate_installer_stages(&effective, |_tool| true)
            .expect("present upx must pass even under --require-tools");
        assert_eq!(available, vec![StageId::Build, StageId::Upx]);
    }

    /// Dev mode (no `--require-tools`, upx not in `explicit_stages`): a missing
    /// upx binary must warn-and-DROP, never error — so a dev box lacking upx
    /// stays usable, mirroring the installer family's host-default warn-skip.
    /// The stage's own lenient runtime guard still applies in the child; here
    /// the harness gate simply removes it from the effective set.
    #[test]
    fn dev_mode_warn_skips_host_default_upx_when_tool_absent() {
        let mut h = empty_harness();
        h.explicit_stages = Vec::new();
        h.require_tools = false;
        h.config_tools.insert(StageId::Upx, vec!["upx".to_string()]);
        let effective = vec![StageId::Build, StageId::Upx];
        let available = h
            .gate_installer_stages(&effective, |_tool| false)
            .expect("dev-mode host-default missing upx must warn-and-drop, not error");
        assert_eq!(
            available,
            vec![StageId::Build],
            "missing-tool upx must be dropped in dev mode; non-gated stages pass through"
        );
    }

    /// Release-blocker regression: a `version: v3` MSI (candle+light) on a
    /// Windows shard that HAS candle+light must NOT skip/hard-fail. Before
    /// the fix the gate hardcoded `wix` (the v4 CLI) for `msi` on Windows, so
    /// it probed an absent binary and hard-failed the whole Windows shard on
    /// every release even though the build runs candle+light. Drives the real
    /// gate with the resolved v3 tools present.
    #[test]
    fn msi_v3_gate_passes_when_candle_and_light_present() {
        let mut h = empty_harness();
        h.stages = vec![StageId::Build, StageId::Msi];
        // Resolved v3 tool set; both probe as present.
        h.config_tools.insert(
            StageId::Msi,
            vec!["candle".to_string(), "light".to_string()],
        );
        let available = h
            .gate_installer_stages(&h.stages.clone(), |tool| matches!(tool, "candle" | "light"))
            .expect("v3 msi with candle+light present must pass the gate");
        assert_eq!(
            available,
            vec![StageId::Build, StageId::Msi],
            "msi must stay in the effective set when its resolved tools are present"
        );
    }

    /// The flip side: when the resolved WiX tool is genuinely absent the gate
    /// must still hard-fail an explicitly-requested msi stage, and name the
    /// first missing tool — so a v3 shard missing `light` is caught.
    #[test]
    fn msi_v3_gate_hard_fails_when_a_resolved_tool_absent() {
        let mut h = empty_harness();
        h.stages = vec![StageId::Build, StageId::Msi];
        h.explicit_stages = h.stages.clone();
        h.config_tools.insert(
            StageId::Msi,
            vec!["candle".to_string(), "light".to_string()],
        );
        // `candle` present, `light` missing — v3 needs both, so msi skips.
        let err = h
            .gate_installer_stages(&h.stages.clone(), |tool| tool == "candle")
            .expect_err("v3 msi missing `light` must hard-fail the run");
        let msg = err.to_string();
        assert!(
            msg.contains("msi") && msg.contains("light"),
            "error must name msi and the first missing tool: {msg}"
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

    /// The headroom guard must ABORT before a run when free space is below
    /// the floor, and the error must carry the actionable numbers so a
    /// recurrence is diagnosable from the log alone — never let the harness
    /// limp into the opaque `hdiutil` ENOSPC.
    #[test]
    fn headroom_guard_aborts_below_floor_with_actionable_message() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let mut h = empty_harness();
        h.disk_abs_floor_bytes = 45 * GIB;
        let log = StageLogger::new("test", Verbosity::Quiet);
        let vol = std::path::Path::new("/Volumes/scratch");
        // run-0 (no prior peak), 30 GiB free, 45 GiB floor → abort.
        let err = h
            .guard_run_headroom(&log, 0, vol, Some(30 * GIB), None)
            .expect_err("below-floor free space must abort the run");
        let msg = err.to_string();
        assert!(msg.contains("determinism run 1"), "1-based run: {msg}");
        assert!(
            msg.contains(&format!("{}", 45 * GIB)),
            "exact required: {msg}"
        );
        assert!(
            msg.contains(&format!("{}", 30 * GIB)),
            "exact available: {msg}"
        );
        assert!(msg.contains("/Volumes/scratch"), "volume: {msg}");
        assert!(msg.contains("reclaim-disk"), "remedy hint: {msg}");
        assert!(
            msg.contains("absolute floor"),
            "run-0 must state the floor basis, not a peak guarantee: {msg}"
        );
    }

    /// Ample headroom → the guard proceeds. And run-1's MEASURED-peak gate
    /// (the B1 fix) aborts when a prior run's peak × factor exceeds the
    /// available space, where a net-delta guard would have wrongly proceeded.
    #[test]
    fn headroom_guard_proceeds_with_ample_space_and_gates_on_measured_peak() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let mut h = empty_harness();
        h.disk_abs_floor_bytes = 45 * GIB;
        h.disk_safety_factor = 1.3;
        let log = StageLogger::new("test", Verbosity::Quiet);
        let vol = std::path::Path::new("/scratch");
        // run-0 with 60 GiB free clears the 45 GiB floor.
        assert!(
            h.guard_run_headroom(&log, 0, vol, Some(60 * GIB), None)
                .is_ok(),
            "ample free space must proceed"
        );
        // run-1 gated on run-0's measured PEAK of 70 GiB; ×1.3 = 91 GiB
        // required. 71 GiB free → abort (a net delta would have seen ~30 and
        // proceeded into ENOSPC); 95 GiB free → proceed.
        let prior_peak = Some(70 * GIB);
        assert!(
            h.guard_run_headroom(&log, 1, vol, Some(71 * GIB), prior_peak)
                .is_err(),
            "71 GiB free under a 91 GiB peak-projected requirement must abort"
        );
        assert!(
            h.guard_run_headroom(&log, 1, vol, Some(95 * GIB), prior_peak)
                .is_ok(),
            "95 GiB free clears the 91 GiB peak-projected requirement"
        );
    }

    /// A probe gap (free space unknown) must degrade to a no-op — the guard
    /// never manufactures a failure from missing disk data.
    #[test]
    fn headroom_guard_unknown_free_space_is_noop() {
        let h = empty_harness();
        let log = StageLogger::new("test", Verbosity::Quiet);
        let vol = std::path::Path::new("/scratch");
        assert!(
            h.guard_run_headroom(&log, 1, vol, None, None).is_ok(),
            "unknown free space must proceed (no manufactured abort)"
        );
    }
}
