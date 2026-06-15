//! Installer-tool availability gating for the determinism harness.
//!
//! Pure-CLI helper module: holds the static map from each installer
//! [`super::StageId`] to the list of tool binaries it depends on, plus
//! the [`filter_available_installer_stages`] function that drops any
//! installer stage whose backing tool(s) are not reachable on `PATH`.
//!
//! Why a separate module: the `crates/cli/**` forbid-list bans direct
//! `Command::new` calls. The actual `<tool> --version` probe lives in
//! [`anodizer_core::tool_detect::tool_available`] — this module merely
//! consults that allow-listed probe and decides which installer stages
//! the child release subprocess can usefully run.
//!
//! Behavioral contract: when an operator requests
//! `--stages=installers` (or any single installer stage) and the
//! corresponding tool is missing, the harness must NOT fail. The
//! pipeline would error out at the stage's `Command::new("wix")` (or
//! similar), and a harness that fails to detect that ahead of time
//! would surface as a confusing "stage failed" instead of an honest
//! "tool not installed, stage skipped".

use super::StageId;
use anodizer_core::tool_detect::tool_available;

/// Result of an installer-tool availability sweep.
///
/// `available` carries the stages whose backing tool(s) are reachable
/// on `PATH`; `skipped` carries the (stage, missing-tool-name) pairs
/// the harness will surface to stderr so the operator can install the
/// missing tool.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct InstallerToolGate {
    pub available: Vec<StageId>,
    pub skipped: Vec<(StageId, &'static str)>,
}

/// Tool binaries each installer stage needs reachable on `PATH` to
/// produce its artifact. When the FIRST tool in the list is missing,
/// the stage is dropped from the effective stage set — every stage
/// here treats its primary tool as load-bearing.
///
/// `nfpm` / `makeself` / `rpmbuild` / `makensis` are single-binary
/// stages, so the list has one entry each. `msi` / `dmg` / `pkg` have
/// platform-conditional primaries, picked to match the binary the
/// stage's spawn surface actually invokes on the current host:
///
/// - `msi`: `wix` on Windows, `wixl` (msitools) elsewhere — WiX is
///   Windows-only/EULA-gated and the Linux MSI path is wixl.
/// - `dmg`: `hdiutil` on macOS, `genisoimage` elsewhere (stage-dmg's
///   non-macOS preference is genisoimage > mkisofs).
/// - `pkg`: `pkgbuild` on macOS, `xar` elsewhere (the flat-XAR
///   toolchain's sentinel; pkgbuild is macOS-only).
///
/// The harness's missing-tool detection is best-effort: when a
/// secondary path is the only one installed on a host, the stage will
/// still attempt to run and either succeed or surface the failure at
/// `Command::new` time.
fn stage_primary_tool(stage: StageId) -> Option<&'static str> {
    match stage {
        StageId::Nfpm => Some("nfpm"),
        StageId::Makeself => Some("makeself"),
        StageId::Srpm => Some("rpmbuild"),
        StageId::Msi => Some(if cfg!(target_os = "windows") {
            "wix"
        } else {
            // WiX itself is Windows-only and EULA-gated; stage-msi's
            // non-Windows path is `wixl` (msitools), which anodizer's config
            // forces via `version: wixl` and the action's auto-install
            // provides. Probing `wix` here would wrongly skip the stage on
            // the Linux shard that actually builds the MSI.
            "wixl"
        }),
        StageId::Nsis => Some("makensis"),
        StageId::Dmg => Some(if cfg!(target_os = "macos") {
            "hdiutil"
        } else {
            // stage-dmg's non-macOS preference order is genisoimage > mkisofs
            // (see anodizer_stage_dmg::dmg_tool). The gate probes the
            // preferred binary; if only mkisofs is present the stage still
            // falls back to it at spawn time (gate is best-effort).
            "genisoimage"
        }),
        StageId::Pkg => Some(if cfg!(target_os = "macos") {
            "pkgbuild"
        } else {
            // stage-pkg's non-macOS path is the flat-XAR toolchain
            // (xar+mkbom+cpio); `xar` is its sentinel (resolve_pkg_builder /
            // the ToolAnyOf{pkgbuild,xar} env requirement). pkgbuild is
            // macOS-only, so probing it would wrongly skip the stage on the
            // Linux shard that actually builds the .pkg.
            "xar"
        }),
        _ => None,
    }
}

/// Every installer-family stage the harness recognises. Order matches
/// the surface defined in the module docstring (nfpm before makeself
/// before msi etc.) so the umbrella `--stages=installers` selection
/// produces a stable order in the report's `stages_under_test` array.
///
/// Re-exported under [`super::installer_stages`] so the CLI parser
/// can expand `--stages=installers` against the same source of truth
/// the harness consults — no risk of the two surfaces drifting.
pub fn installer_stages() -> Vec<StageId> {
    vec![
        StageId::Nfpm,
        StageId::Makeself,
        StageId::Srpm,
        StageId::Msi,
        StageId::Nsis,
        StageId::Dmg,
        StageId::Pkg,
    ]
}

/// True iff `stage` is one of the installer-family stages. Used by
/// the unit tests covering [`installer_stages`] to verify every
/// returned entry has a registered primary-tool binary.
#[cfg(test)]
pub(super) fn is_installer_stage(stage: StageId) -> bool {
    stage_primary_tool(stage).is_some()
}

/// Probe each installer stage in `requested` and partition into
/// available vs skipped. Non-installer stages pass through to
/// `available` unmodified.
///
/// The probe path is [`anodizer_core::tool_detect::tool_available`],
/// which runs `<tool> --version` with stdout/stderr silenced. A spawn
/// error (`NotFound`) and a non-zero exit both fold to "tool not
/// available" — the stage's backing `Command::new` would have hit
/// the same outcome at pipeline-execution time.
pub(super) fn filter_available_installer_stages(requested: &[StageId]) -> InstallerToolGate {
    filter_available_with_probe(requested, |tool| tool_available(tool).unwrap_or(false))
}

/// Inner partition function with an injectable probe. Behavioral tests
/// pass a stub to verify the "tool missing => stage lands in
/// `skipped`" contract without depending on what's installed on the
/// runner.
fn filter_available_with_probe<P>(requested: &[StageId], probe: P) -> InstallerToolGate
where
    P: Fn(&str) -> bool,
{
    let mut gate = InstallerToolGate::default();
    for &stage in requested {
        match stage_primary_tool(stage) {
            None => gate.available.push(stage),
            Some(tool) => {
                if probe(tool) {
                    gate.available.push(stage);
                } else {
                    gate.skipped.push((stage, tool));
                }
            }
        }
    }
    gate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installer_stages_covers_every_installer_family() {
        let stages = installer_stages();
        assert_eq!(stages.len(), 7);
        for stage in stages {
            assert!(
                stage_primary_tool(stage).is_some(),
                "installer_stages() emitted non-installer stage {:?}",
                stage
            );
        }
    }

    #[test]
    fn non_installer_stages_pass_through() {
        let req = vec![StageId::Build, StageId::Archive, StageId::Checksum];
        let gate = filter_available_installer_stages(&req);
        assert_eq!(gate.available, req);
        assert!(gate.skipped.is_empty());
    }

    #[test]
    fn well_formed_partition_on_every_requested_stage() {
        // Structural invariant: every requested stage must land in
        // exactly one of `available` or `skipped`. Independent of host
        // tool set.
        let req = installer_stages();
        let gate = filter_available_installer_stages(&req);
        assert_eq!(
            gate.available.len() + gate.skipped.len(),
            req.len(),
            "every requested stage must land in exactly one bucket"
        );
        for (stage, tool) in &gate.skipped {
            assert!(
                is_installer_stage(*stage),
                "skipped entry references non-installer stage {:?}",
                stage
            );
            assert!(!tool.is_empty(), "missing-tool name must be non-empty");
        }
    }

    #[test]
    fn missing_tool_routes_every_installer_to_skipped() {
        // Behavioral contract: with an always-false probe (every tool
        // missing), every installer stage must land in `skipped` paired
        // with its expected primary-tool name. Non-installer stages
        // must still pass through to `available`.
        let req = vec![
            StageId::Build, // pass-through (no primary tool)
            StageId::Nfpm,
            StageId::Makeself,
            StageId::Msi,
            StageId::Dmg,
            StageId::Pkg,
            StageId::Archive, // pass-through
        ];
        let gate = filter_available_with_probe(&req, |_| false);
        assert_eq!(
            gate.available,
            vec![StageId::Build, StageId::Archive],
            "non-installer stages must pass through even with missing probe"
        );
        let skipped_stages: Vec<StageId> = gate.skipped.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            skipped_stages,
            vec![
                StageId::Nfpm,
                StageId::Makeself,
                StageId::Msi,
                StageId::Dmg,
                StageId::Pkg
            ],
            "installer stages must land in `skipped` when their tool is missing"
        );
        // msi/dmg/pkg resolve their primary tool per host so the Linux
        // determinism shard (which actually builds these installers via the
        // wixl/genisoimage/xar fallbacks) probes the binary it installs, not
        // the macOS/Windows-native one.
        let msi_tool = if cfg!(target_os = "windows") {
            "wix"
        } else {
            "wixl"
        };
        let dmg_tool = if cfg!(target_os = "macos") {
            "hdiutil"
        } else {
            "genisoimage"
        };
        let pkg_tool = if cfg!(target_os = "macos") {
            "pkgbuild"
        } else {
            "xar"
        };
        assert_eq!(
            gate.skipped.iter().map(|(_, t)| *t).collect::<Vec<_>>(),
            vec!["nfpm", "makeself", msi_tool, dmg_tool, pkg_tool],
            "each skipped entry must carry its primary-tool name"
        );
    }

    #[test]
    fn linux_installer_gate_probes_the_linux_fallback_tools() {
        // The determinism installer shard runs on Linux. The gate must probe
        // the Linux-native fallback binaries (wixl/genisoimage/xar), never the
        // macOS/Windows-native ones (wix/hdiutil/pkgbuild) that the shard does
        // not install — otherwise these stages silently route to `skipped` and
        // never get byte-verified. This pins that mapping on the Linux build.
        #[cfg(target_os = "linux")]
        {
            assert_eq!(stage_primary_tool(StageId::Msi), Some("wixl"));
            assert_eq!(stage_primary_tool(StageId::Dmg), Some("genisoimage"));
            assert_eq!(stage_primary_tool(StageId::Pkg), Some("xar"));
            assert_eq!(stage_primary_tool(StageId::Srpm), Some("rpmbuild"));
            assert_eq!(stage_primary_tool(StageId::Nsis), Some("makensis"));
        }
    }

    #[test]
    fn present_tool_routes_every_installer_to_available() {
        // Behavioral contract: with an always-true probe (every tool
        // installed), every installer stage must land in `available`.
        let req = installer_stages();
        let gate = filter_available_with_probe(&req, |_| true);
        assert_eq!(gate.available, req);
        assert!(gate.skipped.is_empty());
    }

    #[test]
    fn is_installer_stage_matches_primary_tool_table() {
        for stage in installer_stages() {
            assert!(is_installer_stage(stage));
        }
        for stage in [
            StageId::Build,
            StageId::Source,
            StageId::Archive,
            StageId::Sbom,
            StageId::Sign,
            StageId::Checksum,
            StageId::CargoPackage,
            StageId::Docker,
            StageId::Snapcraft,
            StageId::Upx,
        ] {
            assert!(
                !is_installer_stage(stage),
                "is_installer_stage({:?}) should be false",
                stage
            );
        }
    }
}
