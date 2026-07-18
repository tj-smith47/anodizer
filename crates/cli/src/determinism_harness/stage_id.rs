use super::*;

/// Stage subset selector for `--stages=<subset>`.
///
/// Currently informational: every variant maps to "run the build-side
/// pipeline and look at the artifacts that stage produces". The harness
/// shells to `anodize release --snapshot --skip=...` which runs the full
/// build-side pipeline; finer-grained per-stage gating is a follow-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, EnumIter)]
pub enum StageId {
    Build,
    Source,
    Upx,
    Archive,
    Nfpm,
    Makeself,
    /// `curl | sh` installer-script reproducibility probe.
    ///
    /// Drives the `anodizer_stage_install_script` crate, which derives its
    /// case tables from configured release intent (via
    /// `anodizer_core::installer::render_installer_cases`) and only writes a
    /// text file — no external tool, no read of produced artifacts. It is
    /// therefore byte-identical on every shard regardless of which binaries
    /// that shard compiled, so it needs no gating tool and no build.
    InstallScript,
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
            StageId::InstallScript => "install-script",
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

    /// The [`StageId`] whose [`Self::as_str`] token equals `token`, or `None`
    /// for an unrecognized token.
    ///
    /// Exact inverse of [`Self::as_str`], derived from it by scanning
    /// [`Self::iter`] rather than a second match — the token vocabulary lives
    /// in [`Self::as_str`] alone, so the parser, the `Known stages:` hint, and
    /// this inverse cannot drift from the enum.
    pub fn from_token(token: &str) -> Option<Self> {
        Self::iter().find(|s| s.as_str() == token)
    }
}
