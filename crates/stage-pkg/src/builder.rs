use anyhow::Result;

// ---------------------------------------------------------------------------
// pkgbuild_command
// ---------------------------------------------------------------------------

/// Construct the `pkgbuild` CLI command arguments.
///
/// Returns args suitable for `Command::new(&args[0]).args(&args[1..])`.
pub fn pkgbuild_command(
    staging_dir: &str,
    identifier: &str,
    version: &str,
    install_location: &str,
    scripts: Option<&str>,
    min_os_version: Option<&str>,
    output_path: &str,
) -> Vec<String> {
    let mut args = vec![
        "pkgbuild".to_string(),
        "--root".to_string(),
        staging_dir.to_string(),
        "--identifier".to_string(),
        identifier.to_string(),
        "--version".to_string(),
        version.to_string(),
        "--install-location".to_string(),
        install_location.to_string(),
    ];

    if let Some(scripts_dir) = scripts {
        args.push("--scripts".to_string());
        args.push(scripts_dir.to_string());
    }

    if let Some(min_os) = min_os_version {
        args.push("--min-os-version".to_string());
        args.push(min_os.to_string());
    }

    args.push(output_path.to_string());
    args
}

// ---------------------------------------------------------------------------
// Tool resolution
// ---------------------------------------------------------------------------

/// Which build path produces the flat `.pkg`.
///
/// Resolved once per config entry from PATH so the per-binary loop can dispatch
/// without re-probing. `pkgbuild` (Apple/Xcode, macOS-only) is preferred when
/// present; otherwise the Linux flat-package toolchain (`xar` + `mkbom` +
/// `cpio`, with gzip done in-process) assembles the identical XAR layout by
/// hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkgBuilder {
    /// Native Apple `pkgbuild` on PATH.
    Pkgbuild,
    /// Linux-native flat XAR assembly via `xar`/`mkbom`/`cpio` (gzip in-process).
    Linux,
}

/// The Linux flat-package toolchain, all of which must be present together for
/// [`PkgBuilder::Linux`]. gzip is NOT listed: the Payload is gzipped in-process
/// (flate2) so the compressed stream is byte-stable, so no `gzip` binary is
/// spawned or required.
pub(crate) const LINUX_PKG_TOOLS: [&str; 3] = ["xar", "mkbom", "cpio"];

/// Resolve which `.pkg` build path is available on this host.
///
/// `probe` reports whether a tool name resolves on PATH (injectable so the
/// resolution logic is unit-testable without a real PATH). Returns the actionable
/// error string naming BOTH options when neither path is satisfiable.
pub fn resolve_pkg_builder(probe: impl Fn(&str) -> bool) -> Result<PkgBuilder, String> {
    if probe("pkgbuild") {
        return Ok(PkgBuilder::Pkgbuild);
    }
    if LINUX_PKG_TOOLS.iter().all(|t| probe(t)) {
        return Ok(PkgBuilder::Linux);
    }
    Err(
        "neither `pkgbuild` (macOS, `xcode-select --install`) nor the Linux \
         flat-package toolchain (`xar` + `mkbom`/bomutils + `cpio`) is \
         available; install one to build .pkg installers"
            .to_string(),
    )
}

/// Environment requirements for the pkg stage: either `pkgbuild` (macOS) or the
/// Linux flat-package toolchain, when any active `pkgs:` entry exists and the
/// configured build targets include macOS (the stage only packages darwin
/// binaries).
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    if !anodizer_core::env_preflight::configured_build_targets(ctx)
        .iter()
        .any(|t| anodizer_core::target::is_darwin(t))
    {
        return Vec::new();
    }
    let configured = ctx
        .config
        .crate_universe()
        .into_iter()
        .flat_map(|c| c.pkgs.iter().flatten())
        .any(|cfg| {
            !anodizer_core::env_preflight::entry_inactive(
                ctx,
                cfg.skip.as_ref(),
                None,
                cfg.if_condition.as_deref(),
            )
        });
    if !configured {
        return Vec::new();
    }
    // `xar` is the sentinel for the Linux flat-package path: ToolAnyOf is
    // any-of and cannot express "all three of xar+mkbom+cpio together", so
    // preflight surfaces "pkgbuild OR xar"; the build-time resolution in
    // `resolve_pkg_builder` still enforces the full three-tool group and bails
    // with the precise message when only a partial Linux toolchain is present.
    vec![anodizer_core::EnvRequirement::ToolAnyOf {
        names: vec!["pkgbuild".to_string(), "xar".to_string()],
    }]
}
