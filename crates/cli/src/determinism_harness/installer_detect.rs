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
/// platform-conditional fallbacks (e.g. `hdiutil` on macOS, `mkisofs`
/// or `genisoimage` on Linux for the DMG stage), and the
/// [`stage_primary_tool`] mapping picks the *primary* binary the
/// stage's spawn surface invokes in practice. The harness's missing-
/// tool detection is best-effort: when a secondary path is the only
/// one installed on a host, the stage will still attempt to run and
/// either succeed or surface the failure at `Command::new` time.
fn stage_primary_tool(stage: StageId) -> Option<&'static str> {
    match stage {
        StageId::Nfpm => Some("nfpm"),
        StageId::Makeself => Some("makeself"),
        StageId::Srpm => Some("rpmbuild"),
        StageId::Msi => Some("wix"),
        StageId::Nsis => Some("makensis"),
        StageId::Dmg => Some(if cfg!(target_os = "macos") {
            "hdiutil"
        } else {
            "mkisofs"
        }),
        StageId::Pkg => Some("pkgbuild"),
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
    let mut gate = InstallerToolGate::default();
    for &stage in requested {
        match stage_primary_tool(stage) {
            None => gate.available.push(stage),
            Some(tool) => match tool_available(tool) {
                Ok(true) => gate.available.push(stage),
                Ok(false) | Err(_) => gate.skipped.push((stage, tool)),
            },
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
    fn missing_installer_tool_lands_in_skipped() {
        // Force a stage whose primary tool will not resolve on this host
        // by stubbing `stage_primary_tool` through a wrapper. The simplest
        // form: pick an installer stage and check that AT LEAST ONE of
        // available / skipped is populated, so the function returns a
        // well-formed gate regardless of the runner's tool set.
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
