//! Installer-tool availability gating for the determinism harness.
//!
//! Pure-CLI helper module: holds the static map from each installer
//! [`super::StageId`] to the list of tool binaries it depends on, plus
//! the [`filter_available_with_probe`] function that drops any
//! installer stage whose backing tool(s) are not reachable on `PATH`.
//!
//! Why a separate module: the `crates/cli/**` forbid-list bans direct
//! `Command::new` calls. The actual `<tool> --version` probe lives in
//! [`anodizer_core::tool_detect::tool_available`] — this module merely
//! consults that allow-listed probe and decides which installer stages
//! the child release subprocess can usefully run.
//!
//! Behavioral contract: this module only PARTITIONS requested stages
//! into available vs missing-tool. The decision of what to do with a
//! missing-tool stage lives at the harness call site
//! ([`super::Harness::run`]): an EXPLICITLY-requested installer stage
//! (one the operator typed into `--stages`, which is the only way an
//! installer stage enters the set — none are in the default set or
//! auto-included) whose tool is missing is a HARD ERROR, mirroring the
//! docker stage's contract. A silent warn-skip there would be false
//! coverage: a determinism shard claiming it byte-verified a format it
//! then produced nothing for — the exact failure mode that hid the
//! macOS/Windows installer formats from every release. The warn-skip
//! path remains only for any future auto-included installer stage that
//! the operator did NOT explicitly request.

use super::StageId;
use anodizer_core::util::find_binary;

/// Result of an installer-tool availability sweep.
///
/// `available` carries the stages whose backing tool(s) are reachable
/// on `PATH`; `skipped` carries the (stage, missing-tool-name) pairs
/// the harness will surface to stderr so the operator can install the
/// missing tool.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct InstallerToolGate {
    pub available: Vec<StageId>,
    pub skipped: Vec<(StageId, String)>,
}

/// Tool binaries each installer stage needs reachable on `PATH` to
/// produce its artifact. When the FIRST tool in the list is missing,
/// the stage is dropped from the effective stage set — every stage
/// here treats its primary tool as load-bearing.
///
/// `nfpm` / `makeself` / `rpmbuild` / `makensis` are single-binary
/// stages, so the list has one entry each. `dmg` / `pkg` have
/// platform-conditional primaries, picked to match the binary the
/// stage's spawn surface actually invokes on the current host. (The
/// `msi` stage is NOT in this table: its required tool depends on the
/// resolved WiX *version* — v3 runs `candle`+`light`, v4 runs `wix`,
/// the Linux path runs `wixl` — so a host-static guess would drift
/// from the version the build runs. The gate resolves it from config
/// instead; see [`filter_available_with_probe`]'s `msi_tools`.)
///
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

/// The WiX tool requirement for the `msi` stage, resolved from config by
/// the same policy the build runs (explicit `version:` > `.wxs` namespace
/// sniff > installed-tool probe). Distinct from the host-static
/// [`stage_primary_tool`] entries: WiX v3 spawns `candle`+`light`, v4
/// spawns `wix`, the Linux path spawns `wixl`, so the required binary
/// follows the resolved version, never the host OS. The dispatcher
/// resolves this via `anodizer_stage_msi::required_msi_tools` (the same
/// helper env-preflight consults) and threads it into the gate.
///
/// Returned as owned `String`s rather than `&'static str` because the
/// values originate from runtime config resolution. An empty slice means
/// no active MSI config — the stage is then treated as having no tool
/// requirement (it would emit nothing anyway).
fn msi_required_tools(msi_tools: &[String]) -> Vec<&str> {
    msi_tools.iter().map(String::as_str).collect()
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

/// True iff `stage` is one of the installer-family stages. `Msi` is
/// covered explicitly because its tool requirement is resolved from
/// config (not present in the host-static [`stage_primary_tool`] table).
#[cfg(test)]
pub(super) fn is_installer_stage(stage: StageId) -> bool {
    stage == StageId::Msi || stage_primary_tool(stage).is_some()
}

impl InstallerToolGate {
    /// The subset of `skipped` entries whose stage the operator EXPLICITLY
    /// requested (present in `explicit`). Installer stages enter the harness's
    /// stage set only by an explicit `--stages` token — either named directly
    /// (`--stages=msi`) or via the `installers` umbrella, which the parser
    /// expands to the concrete installer `StageId`s before they reach the gate
    /// (see `parse_stages`). Both forms land in `self.stages` as explicit IDs,
    /// so a hit here means a shard claimed it would byte-verify a format whose
    /// tool is missing — false coverage that must hard-fail. Mirrors the docker
    /// stage's hard-fail-when-explicit contract.
    pub(super) fn explicitly_skipped(&self, explicit: &[StageId]) -> Vec<(StageId, String)> {
        self.skipped
            .iter()
            .filter(|(stage, _)| explicit.contains(stage))
            .cloned()
            .collect()
    }
}

/// Build the hard-fail message for explicitly-requested installer stages
/// whose tool is missing. Pure (no I/O) so the message contract is unit-
/// testable without constructing a full [`super::Harness`].
pub(super) fn missing_tool_error(skipped: &[(StageId, String)]) -> String {
    let detail = skipped
        .iter()
        .map(|(stage, tool)| format!("`{}` (needs `{}`)", stage.as_str(), tool))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "installer stage(s) requested via --stages but their tool is not on PATH: {detail}. \
         The determinism gate cannot byte-verify these formats, and a silent skip would be \
         false coverage (the exact failure mode that hid the macOS/Windows installers from \
         every release). Provision the missing tool on this shard (e.g. choco install \
         wixtoolset nsis on Windows) or remove the stage from --stages."
    )
}

/// The production tool probe: PATH-existence via
/// [`anodizer_core::util::find_binary`]. The gate's question is strictly
/// "can the stage spawn this binary" — i.e. is it reachable on `PATH` — NOT
/// "does `<tool> --version` exit zero". Several installer tools answer the
/// latter with a non-zero exit despite being present and runnable: `hdiutil`
/// has no version flag at all, and `pkgbuild` / WiX `candle` / `light` print
/// usage and exit non-zero on `--version`. A `--version` probe therefore
/// reports a present tool as missing and hard-fails the shard (the macOS
/// dmg/pkg gate did exactly this). A binary that is on `PATH` but genuinely
/// broken still surfaces at `Command::new` time, so PATH-existence is both
/// correct for the gate and strictly safer than the version probe.
/// [`super::Harness::run`] injects this into
/// [`super::Harness::gate_installer_stages`]; tests inject a stub.
pub(super) fn host_tool_probe(tool: &str) -> bool {
    find_binary(tool)
}

/// Probe each installer stage in `requested` with `probe` and partition
/// into available vs skipped. Non-installer stages pass through to
/// `available` unmodified. Behavioral tests pass a stub probe to verify
/// the "tool missing => stage lands in `skipped`" contract without
/// depending on what's installed on the runner.
///
/// `msi_tools` carries the WiX binaries the `msi` stage needs, resolved
/// from config by the dispatcher (see [`msi_required_tools`]). The MSI
/// stage is available only when EVERY one is reachable — WiX v3 spawns
/// both `candle` and `light`, so either one missing must skip the stage.
/// The skipped entry reports the first missing tool. An empty `msi_tools`
/// means no active MSI config: the stage carries no requirement and passes
/// through.
pub(super) fn filter_available_with_probe<P>(
    requested: &[StageId],
    msi_tools: &[String],
    probe: P,
) -> InstallerToolGate
where
    P: Fn(&str) -> bool,
{
    let mut gate = InstallerToolGate::default();
    for &stage in requested {
        if stage == StageId::Msi {
            let required = msi_required_tools(msi_tools);
            match required.iter().find(|tool| !probe(tool)) {
                None => gate.available.push(stage),
                Some(missing) => gate.skipped.push((stage, missing.to_string())),
            }
            continue;
        }
        match stage_primary_tool(stage) {
            None => gate.available.push(stage),
            Some(tool) => {
                if probe(tool) {
                    gate.available.push(stage);
                } else {
                    gate.skipped.push((stage, tool.to_string()));
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
            // `Msi`'s tool requirement is config-resolved (not in the
            // host-static `stage_primary_tool` table), so assert via the
            // installer-family predicate that recognises it.
            assert!(
                is_installer_stage(stage),
                "installer_stages() emitted non-installer stage {:?}",
                stage
            );
        }
    }

    #[test]
    fn non_installer_stages_pass_through() {
        let req = vec![StageId::Build, StageId::Archive, StageId::Checksum];
        let gate = filter_available_with_probe(&req, &[], host_tool_probe);
        assert_eq!(gate.available, req);
        assert!(gate.skipped.is_empty());
    }

    #[test]
    fn well_formed_partition_on_every_requested_stage() {
        // Structural invariant: every requested stage must land in
        // exactly one of `available` or `skipped`. Independent of host
        // tool set. A resolved v3 msi tool set is supplied so the msi
        // stage carries a concrete requirement.
        let req = installer_stages();
        let msi_tools = vec!["candle".to_string(), "light".to_string()];
        let gate = filter_available_with_probe(&req, &msi_tools, host_tool_probe);
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
        // Config resolved msi to WiX v3 (candle+light); with every tool
        // missing the first one (`candle`) is reported.
        let msi_tools = vec!["candle".to_string(), "light".to_string()];
        let gate = filter_available_with_probe(&req, &msi_tools, |_| false);
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
        // dmg/pkg resolve their primary tool per host so the Linux
        // determinism shard (which actually builds these installers via the
        // genisoimage/xar fallbacks) probes the binary it installs, not the
        // macOS-native one. msi's tool comes from the resolved WiX version.
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
            gate.skipped
                .iter()
                .map(|(_, t)| t.as_str())
                .collect::<Vec<_>>(),
            vec!["nfpm", "makeself", "candle", dmg_tool, pkg_tool],
            "each skipped entry must carry its missing-tool name"
        );
    }

    #[test]
    fn linux_installer_gate_probes_the_linux_fallback_tools() {
        // The determinism installer shard runs on Linux. The host-static gate
        // entries must probe the Linux-native fallback binaries
        // (genisoimage/xar), never the macOS-native ones (hdiutil/pkgbuild)
        // that the shard does not install — otherwise these stages silently
        // route to `skipped` and never get byte-verified. (msi is config-
        // resolved, not host-static — see the WiX-version gate tests.) This
        // pins the host-static mapping on the Linux build.
        #[cfg(target_os = "linux")]
        {
            assert_eq!(stage_primary_tool(StageId::Dmg), Some("genisoimage"));
            assert_eq!(stage_primary_tool(StageId::Pkg), Some("xar"));
            assert_eq!(stage_primary_tool(StageId::Srpm), Some("rpmbuild"));
            assert_eq!(stage_primary_tool(StageId::Nsis), Some("makensis"));
            // msi has no host-static entry; its tool follows the resolved
            // WiX version threaded in from config.
            assert_eq!(stage_primary_tool(StageId::Msi), None);
        }
    }

    #[test]
    fn present_tool_routes_every_installer_to_available() {
        // Behavioral contract: with an always-true probe (every tool
        // installed), every installer stage must land in `available`.
        let req = installer_stages();
        let msi_tools = vec!["candle".to_string(), "light".to_string()];
        let gate = filter_available_with_probe(&req, &msi_tools, |_| true);
        assert_eq!(gate.available, req);
        assert!(gate.skipped.is_empty());
    }

    #[test]
    fn msi_v3_available_when_candle_and_light_present() {
        // Release-blocker regression: config pins WiX v3 (candle+light) and
        // both are on PATH (the real Windows shard). The gate must NOT skip
        // msi. Before the fix it hardcoded the v4 `wix` CLI on Windows, found
        // it absent, and hard-failed the shard on every release.
        let msi_tools = vec!["candle".to_string(), "light".to_string()];
        let gate = filter_available_with_probe(&[StageId::Build, StageId::Msi], &msi_tools, |t| {
            matches!(t, "candle" | "light")
        });
        assert_eq!(
            gate.available,
            vec![StageId::Build, StageId::Msi],
            "v3 msi must stay available when candle+light are present"
        );
        assert!(gate.skipped.is_empty(), "no tool is missing");
    }

    #[test]
    fn msi_v3_skips_when_one_resolved_tool_absent() {
        // WiX v3 spawns BOTH candle and light — either one missing must skip
        // the stage, and the skipped entry reports the first missing tool.
        let msi_tools = vec!["candle".to_string(), "light".to_string()];
        let gate = filter_available_with_probe(&[StageId::Msi], &msi_tools, |t| t == "candle");
        assert!(gate.available.is_empty());
        assert_eq!(
            gate.skipped,
            vec![(StageId::Msi, "light".to_string())],
            "the first missing resolved tool (`light`) must be reported"
        );
    }

    #[test]
    fn msi_v4_skips_when_wix_absent() {
        // A v4 config resolves to the single `wix` CLI; absent → skip.
        let msi_tools = vec!["wix".to_string()];
        let gate = filter_available_with_probe(&[StageId::Msi], &msi_tools, |_| false);
        assert_eq!(gate.skipped, vec![(StageId::Msi, "wix".to_string())]);
    }

    #[test]
    fn msi_available_when_no_active_config_yields_empty_tools() {
        // Empty `msi_tools` means no active MSI config: the stage carries no
        // requirement and must pass through to `available` even under an
        // always-false probe (there is nothing to be missing).
        let gate = filter_available_with_probe(&[StageId::Msi], &[], |_| false);
        assert_eq!(gate.available, vec![StageId::Msi]);
        assert!(gate.skipped.is_empty());
    }

    #[test]
    fn explicitly_skipped_filters_to_operator_requested_stages() {
        // Two installers skipped (tool missing), but only `msi` was in the
        // operator's explicit `--stages`. The hard-fail set must contain msi
        // and NOT dmg — a missing tool for a stage the operator did not type
        // is a warn-skip, not a release-blocking hard error.
        let msi_tools = vec!["wixl".to_string()];
        let gate =
            filter_available_with_probe(&[StageId::Msi, StageId::Dmg], &msi_tools, |_| false);
        let explicit = vec![StageId::Msi, StageId::Build];
        let hard = gate.explicitly_skipped(&explicit);
        let stages: Vec<StageId> = hard.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            stages,
            vec![StageId::Msi],
            "only the explicitly-requested missing-tool stage hard-fails"
        );
    }

    #[test]
    fn explicitly_skipped_empty_when_nothing_requested() {
        let msi_tools = vec!["wixl".to_string()];
        let gate =
            filter_available_with_probe(&[StageId::Msi, StageId::Nsis], &msi_tools, |_| false);
        assert!(
            gate.explicitly_skipped(&[]).is_empty(),
            "no explicit request => no hard-fail even when tools are missing"
        );
    }

    #[test]
    fn missing_tool_error_names_every_stage_and_tool() {
        let msg = missing_tool_error(&[
            (StageId::Msi, "candle".to_string()),
            (StageId::Nsis, "makensis".to_string()),
        ]);
        assert!(msg.contains("msi") && msg.contains("candle"), "msg: {msg}");
        assert!(
            msg.contains("nsis") && msg.contains("makensis"),
            "msg: {msg}"
        );
        assert!(
            msg.contains("--stages"),
            "message must tell the operator how to opt out: {msg}"
        );
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

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn host_tool_probe_detects_present_tool_that_lacks_version_flag() {
        // Regression: the gate detects a tool by PATH-existence, NOT by a
        // `<tool> --version` exit code. `hdiutil` (macOS) has no version flag
        // and exits non-zero on `--version`; the old `tool_available` probe
        // therefore reported it missing and hard-failed the macOS dmg/pkg
        // determinism shard though the binary was present. A stub that exits
        // non-zero on every call models such a tool — `host_tool_probe` (now
        // backed by `find_binary`, a pure PATH lookup with no exec) must still
        // find it.
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;

        let tools = FakeToolDir::new();
        tools.tool("hdiutil").exit(1).install();
        let _path = tools.activate();

        assert!(
            host_tool_probe("hdiutil"),
            "a tool present on PATH must be detected even when it fails --version"
        );
        assert!(
            !host_tool_probe("anodizer-no-such-tool-zzz"),
            "a genuinely absent tool must still report missing"
        );
    }
}
