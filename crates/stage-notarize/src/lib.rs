use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{MacOSNativeSignNotarizeConfig, MacOSSignNotarizeConfig, StringOrBool};
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// Helper: refresh artifact checksums after signing
// ---------------------------------------------------------------------------

/// Re-compute SHA256 for all darwin Binary/UniversalBinary artifacts whose
/// files may have been modified by signing. Updates the `sha256` metadata
/// field in-place (GoReleaser parity: macos.go:144 calls `binaries.Refresh()`).
fn refresh_artifact_checksums(ctx: &mut Context, log: &anodize_core::log::StageLogger) {
    for artifact in ctx.artifacts.all_mut() {
        if !matches!(
            artifact.kind,
            ArtifactKind::Binary | ArtifactKind::UniversalBinary
        ) {
            continue;
        }
        let is_darwin = artifact
            .target
            .as_deref()
            .map(anodize_core::target::is_darwin)
            .unwrap_or(false);
        if !is_darwin {
            continue;
        }
        // Only refresh if sha256 metadata was previously set
        if !artifact.metadata.contains_key("sha256") {
            continue;
        }
        match anodize_core::hashing::sha256_file(&artifact.path) {
            Ok(new_sha) => {
                artifact.metadata.insert("sha256".to_string(), new_sha);
            }
            Err(e) => {
                log.warn(&format!(
                    "notarize: failed to refresh sha256 for {}: {}",
                    artifact.path.display(),
                    e
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: check if a StringOrBool-typed `enabled` field is active
// ---------------------------------------------------------------------------

/// Returns `true` when the config entry is enabled.
///
/// Note: notarize uses an opt-in `enabled` field (default `false`) rather than
/// the opt-out `disable` field used by most other stages. This matches the
/// GoReleaser schema where notarization must be explicitly enabled.
///
/// The GoReleaser schema defaults `enabled` to `false`, so:
/// - `None` → disabled
/// - `Some(Bool(false))` → disabled
/// - `Some(Bool(true))` → enabled
/// - `Some(String(tmpl))` → render template, enabled if result is "true"
fn is_enabled(enabled: &Option<StringOrBool>, ctx: &Context) -> bool {
    match enabled {
        None => false,
        Some(sob) => {
            if sob.is_template() {
                ctx.render_template(sob.as_str())
                    .map(|r| r.trim() == "true")
                    .unwrap_or(false)
            } else {
                sob.as_bool()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: render an optional template field
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Helper: filter artifacts by ids list
// ---------------------------------------------------------------------------

use anodize_core::artifact::matches_id_filter;

/// Check whether an artifact matches the given ids filter — delegates to the
/// canonical `anodize_core::artifact::matches_id_filter` (GoReleaser `ByID`).
fn matches_ids(artifact: &Artifact, ids: &Option<Vec<String>>) -> bool {
    matches_id_filter(artifact, ids.as_deref())
}

// ---------------------------------------------------------------------------
// Helper: redact sensitive values from command args for safe logging
// ---------------------------------------------------------------------------

/// Redact sensitive values from command args for safe logging.
fn redact_args(args: &[String]) -> Vec<String> {
    let sensitive_flags = ["--p12-password", "--api-key-path"];
    let mut result = Vec::with_capacity(args.len());
    let mut redact_next = false;
    for arg in args {
        if redact_next {
            result.push("[REDACTED]".to_string());
            redact_next = false;
        } else if sensitive_flags.iter().any(|f| arg == *f) {
            result.push(arg.clone());
            redact_next = true;
        } else {
            result.push(arg.clone());
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Helper: parse notarize output for status differentiation
// ---------------------------------------------------------------------------

/// Check notarization subprocess output, differentiating between rejected,
/// invalid, timeout, and accepted statuses (GoReleaser parity: macos.go
/// differentiates AcceptedStatus, InvalidStatus, RejectedStatus, TimeoutStatus).
fn check_notarize_output(
    output: &std::process::Output,
    label: &str,
    log: &anodize_core::log::StageLogger,
) -> Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);
    let combined_lower = combined.to_lowercase();

    if output.status.success() {
        // Even on success, check for status keywords to provide accurate logging
        if combined_lower.contains("status: accepted") || combined_lower.contains("status: success")
        {
            log.status(&format!("notarize: {} succeeded (accepted)", label));
        } else if combined_lower.contains("timeout") {
            // GoReleaser treats timeout as non-fatal (logs info, no error)
            log.warn(&format!(
                "notarize: {} timed out (submission may still be processing)",
                label
            ));
        }
        return Ok(());
    }

    // Non-zero exit: differentiate error type from output
    if combined_lower.contains("status: invalid") || combined_lower.contains("invalid submission") {
        bail!(
            "notarize: {}: invalid — the submitted artifact did not pass Apple's checks",
            label
        );
    }
    if combined_lower.contains("status: rejected") || combined_lower.contains("submission rejected")
    {
        bail!(
            "notarize: {}: rejected — Apple rejected the notarization request",
            label
        );
    }
    if combined_lower.contains("timeout") || combined_lower.contains("timed out") {
        // GoReleaser treats timeout as non-fatal (info log, not error)
        log.warn(&format!(
            "notarize: {} timed out waiting for Apple response (submission may still be processing)",
            label
        ));
        return Ok(());
    }

    // Generic failure
    bail!(
        "notarize: {} failed (exit code: {:?})\n{}",
        label,
        output.status.code(),
        combined.trim()
    );
}

// ---------------------------------------------------------------------------
// NotarizeStage
// ---------------------------------------------------------------------------

pub struct NotarizeStage;

impl Stage for NotarizeStage {
    fn name(&self) -> &str {
        "notarize"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("notarize");
        let dry_run = ctx.options.dry_run;

        let notarize_config = match ctx.config.notarize {
            Some(ref cfg) => cfg,
            None => return Ok(()),
        };

        // Respect top-level disable flag
        if let Some(ref d) = notarize_config.disable
            && d.is_disabled(|s| ctx.render_template(s))
        {
            log.status("notarization disabled");
            return Ok(());
        }

        // Phase 1: Cross-platform signing/notarization (rcodesign)
        if let Some(ref macos_configs) = notarize_config.macos {
            for (idx, cfg) in macos_configs.iter().enumerate() {
                run_cross_platform(ctx, cfg, idx, dry_run, &log)?;
            }
        }

        // Phase 2: Native signing/notarization (codesign + xcrun notarytool)
        if let Some(ref native_configs) = notarize_config.macos_native {
            for (idx, cfg) in native_configs.iter().enumerate() {
                run_native(ctx, cfg, idx, dry_run, &log)?;
            }
        }

        // Refresh artifact checksums after signing (GoReleaser parity: macos.go:144).
        // Signing modifies binaries in-place, so SHA256 metadata becomes stale.
        if !dry_run {
            refresh_artifact_checksums(ctx, &log);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Phase 1: Cross-platform (rcodesign)
// ---------------------------------------------------------------------------

fn run_cross_platform(
    ctx: &Context,
    cfg: &MacOSSignNotarizeConfig,
    idx: usize,
    dry_run: bool,
    log: &anodize_core::log::StageLogger,
) -> Result<()> {
    if !is_enabled(&cfg.enabled, ctx) {
        log.status(&format!("notarize: macos[{idx}] skipped (not enabled)"));
        return Ok(());
    }

    // Validate and render sign config
    let sign = cfg
        .sign
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("notarize: macos[{idx}] requires a 'sign' configuration"))?;

    let certificate = ctx
        .render_template_opt(sign.certificate.as_deref())
        .with_context(|| format!("notarize: macos[{idx}] render sign.certificate"))?
        .ok_or_else(|| anyhow::anyhow!("notarize: macos[{idx}] sign.certificate is required"))?;

    let password = ctx
        .render_template_opt(sign.password.as_deref())
        .with_context(|| format!("notarize: macos[{idx}] render sign.password"))?
        .ok_or_else(|| anyhow::anyhow!("notarize: macos[{idx}] sign.password is required"))?;

    let entitlements = ctx
        .render_template_opt(sign.entitlements.as_deref())
        .with_context(|| format!("notarize: macos[{idx}] render sign.entitlements"))?;

    // Render and validate notarize config fields (if present)
    let notarize_api = if let Some(ref ncfg) = cfg.notarize {
        let issuer_id = ctx.render_template_opt(ncfg.issuer_id.as_deref())
            .with_context(|| format!("notarize: macos[{idx}] render notarize.issuer_id"))?
            .ok_or_else(|| {
                anyhow::anyhow!("notarize: macos[{idx}] notarize.issuer_id is required when notarize block is present")
            })?;
        let key = ctx
            .render_template_opt(ncfg.key.as_deref())
            .with_context(|| format!("notarize: macos[{idx}] render notarize.key"))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "notarize: macos[{idx}] notarize.key is required when notarize block is present"
                )
            })?;
        let key_id = ctx.render_template_opt(ncfg.key_id.as_deref())
            .with_context(|| format!("notarize: macos[{idx}] render notarize.key_id"))?
            .ok_or_else(|| {
                anyhow::anyhow!("notarize: macos[{idx}] notarize.key_id is required when notarize block is present")
            })?;
        // Default timeout to 10 minutes (GoReleaser parity: macos.go:33)
        let timeout = ncfg.timeout.clone().or_else(|| Some("10m".to_string()));
        Some((issuer_id, key, key_id, ncfg.wait.unwrap_or(false), timeout))
    } else {
        None
    };

    // Default IDs to project name when not specified (GoReleaser parity: macos.go:35)
    let ids = cfg.ids.clone().or_else(|| {
        if ctx.config.project_name.is_empty() {
            None
        } else {
            Some(vec![ctx.config.project_name.clone()])
        }
    });

    // Collect darwin Binary + UniversalBinary artifacts, filtered by ids
    let darwin_artifacts: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| {
            matches!(a.kind, ArtifactKind::Binary | ArtifactKind::UniversalBinary)
                && a.target
                    .as_deref()
                    .map(anodize_core::target::is_darwin)
                    .unwrap_or(false)
                && matches_ids(a, &ids)
        })
        .collect();

    if darwin_artifacts.is_empty() {
        // Surface the filter contents so misconfigured `ids:` is visible
        // instead of producing a silent no-op.
        log.warn(&format!(
            "notarize: macos[{idx}] ids={:?} matched no darwin binaries \
             (check for typos or unbuilt darwin targets)",
            ids
        ));
        ctx.strict_guard(
            log,
            &format!("notarize: macos[{idx}] no matching darwin binaries found"),
        )?;
        return Ok(());
    }

    for artifact in &darwin_artifacts {
        let binary_path = artifact.path.to_string_lossy();

        // Build rcodesign sign command
        let mut sign_args = vec![
            "rcodesign".to_string(),
            "sign".to_string(),
            "--p12-file".to_string(),
            certificate.clone(),
            "--p12-password".to_string(),
            password.clone(),
            // Apple's public timestamp server (GoReleaser parity: macos.go:95)
            "--timestamp-url".to_string(),
            "http://timestamp.apple.com/ts01".to_string(),
        ];
        if let Some(ref ent) = entitlements {
            sign_args.push("--entitlements-xml-path".to_string());
            sign_args.push(ent.clone());
        }
        sign_args.push(binary_path.to_string());

        log.status(&format!(
            "notarize: signing {} with rcodesign",
            artifact.name()
        ));

        if dry_run {
            log.status(&format!(
                "  [dry-run] would run: {}",
                redact_args(&sign_args).join(" ")
            ));
        } else {
            let status = Command::new(&sign_args[0])
                .args(&sign_args[1..])
                .status()
                .with_context(|| {
                    format!(
                        "notarize: failed to execute rcodesign sign for {}",
                        artifact.name()
                    )
                })?;
            if !status.success() {
                bail!(
                    "notarize: rcodesign sign failed for {} (exit code: {:?})",
                    artifact.name(),
                    status.code()
                );
            }
        }

        // Notarize if configured
        if let Some((ref issuer_id, ref key, ref key_id, wait, ref timeout)) = notarize_api {
            let mut notarize_args = vec![
                "rcodesign".to_string(),
                "notary-submit".to_string(),
                "--api-issuer".to_string(),
                issuer_id.clone(),
                "--api-key".to_string(),
                key_id.clone(),
                "--api-key-path".to_string(),
                key.clone(),
            ];
            if wait {
                notarize_args.push("--wait".to_string());
                if let Some(t) = timeout {
                    notarize_args.push("--max-wait".to_string());
                    notarize_args.push(t.clone());
                }
            }
            notarize_args.push(binary_path.to_string());

            log.status(&format!(
                "notarize: submitting {} for notarization via rcodesign",
                artifact.name()
            ));

            if dry_run {
                log.status(&format!(
                    "  [dry-run] would run: {}",
                    redact_args(&notarize_args).join(" ")
                ));
            } else {
                let output = Command::new(&notarize_args[0])
                    .args(&notarize_args[1..])
                    .output()
                    .with_context(|| {
                        format!(
                            "notarize: failed to execute rcodesign notary-submit for {}",
                            artifact.name()
                        )
                    })?;
                check_notarize_output(
                    &output,
                    &format!("rcodesign notary-submit for {}", artifact.name()),
                    log,
                )?;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 2: Native (codesign + xcrun notarytool)
// ---------------------------------------------------------------------------

/// Parameters for native signing/notarization, extracted from config before
/// calling the mode-specific functions. Avoids passing many positional args
/// (clippy::too_many_arguments).
struct NativeSignParams<'a> {
    idx: usize,
    identity: &'a str,
    keychain: Option<&'a str>,
    options: Option<&'a [String]>,
    entitlements: Option<&'a str>,
    profile_name: &'a str,
    wait: bool,
    timeout: Option<&'a str>,
    ids: &'a Option<Vec<String>>,
}

fn run_native(
    ctx: &Context,
    cfg: &MacOSNativeSignNotarizeConfig,
    idx: usize,
    dry_run: bool,
    log: &anodize_core::log::StageLogger,
) -> Result<()> {
    if !is_enabled(&cfg.enabled, ctx) {
        log.status(&format!(
            "notarize: macos_native[{idx}] skipped (not enabled)"
        ));
        return Ok(());
    }

    let artifact_type = cfg.use_.as_deref().unwrap_or("dmg");

    // Validate sign config
    let sign = cfg.sign.as_ref().ok_or_else(|| {
        anyhow::anyhow!("notarize: macos_native[{idx}] requires a 'sign' configuration")
    })?;

    let identity = ctx
        .render_template_opt(sign.identity.as_deref())
        .with_context(|| format!("notarize: macos_native[{idx}] render sign.identity"))?
        .ok_or_else(|| {
            anyhow::anyhow!("notarize: macos_native[{idx}] sign.identity is required")
        })?;

    let keychain = ctx
        .render_template_opt(sign.keychain.as_deref())
        .with_context(|| format!("notarize: macos_native[{idx}] render sign.keychain"))?;

    let entitlements = ctx
        .render_template_opt(sign.entitlements.as_deref())
        .with_context(|| format!("notarize: macos_native[{idx}] render sign.entitlements"))?;

    // Validate notarize config
    let notarize = cfg.notarize.as_ref().ok_or_else(|| {
        anyhow::anyhow!("notarize: macos_native[{idx}] requires a 'notarize' configuration")
    })?;

    let profile_name = ctx
        .render_template_opt(notarize.profile_name.as_deref())
        .with_context(|| format!("notarize: macos_native[{idx}] render notarize.profile_name"))?
        .ok_or_else(|| {
            anyhow::anyhow!("notarize: macos_native[{idx}] notarize.profile_name is required")
        })?;

    let wait = notarize.wait.unwrap_or(false);

    // Default timeout to 10 minutes (GoReleaser parity: macos.go:33)
    let timeout = ctx
        .render_template_opt(notarize.timeout.as_deref())
        .with_context(|| format!("notarize: macos_native[{idx}] render notarize.timeout"))?
        .or_else(|| Some("10m".to_string()));

    // Default IDs to project name when not specified (GoReleaser parity: macos.go:35)
    let ids = cfg.ids.clone().or_else(|| {
        if ctx.config.project_name.is_empty() {
            None
        } else {
            Some(vec![ctx.config.project_name.clone()])
        }
    });

    // Issue 9: Warn if options set with use: pkg (options only apply to DMGs)
    if artifact_type == "pkg" && sign.options.as_ref().is_some_and(|o| !o.is_empty()) {
        log.warn(&format!(
            "notarize: macos_native[{idx}] sign.options is set but only applies to DMG mode; ignored for pkg"
        ));
    }

    let params = NativeSignParams {
        idx,
        identity: &identity,
        keychain: keychain.as_deref(),
        options: sign.options.as_deref(),
        entitlements: entitlements.as_deref(),
        profile_name: &profile_name,
        wait,
        timeout: timeout.as_deref(),
        ids: &ids,
    };

    match artifact_type {
        "dmg" => run_native_dmg(ctx, &params, dry_run, log),
        "pkg" => run_native_pkg(ctx, &params, dry_run, log),
        other => bail!("notarize: macos_native[{idx}] unsupported artifact type: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Native DMG mode
// ---------------------------------------------------------------------------

fn run_native_dmg(
    ctx: &Context,
    params: &NativeSignParams,
    dry_run: bool,
    log: &anodize_core::log::StageLogger,
) -> Result<()> {
    let idx = params.idx;

    // Step 1: Find AppBundle (Installer with format=appbundle) artifacts for darwin targets
    let app_bundles: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| {
            a.kind == ArtifactKind::Installer
                && a.metadata.get("format").map(|f| f.as_str()) == Some("appbundle")
                && a.target
                    .as_deref()
                    .map(anodize_core::target::is_darwin)
                    .unwrap_or(false)
                && matches_ids(a, params.ids)
        })
        .collect();

    // Sign each app bundle with codesign
    for bundle in &app_bundles {
        let bundle_path = bundle.path.to_string_lossy();

        let mut codesign_args = vec![
            "codesign".to_string(),
            "--deep".to_string(),
            "--force".to_string(),
            "--sign".to_string(),
            params.identity.to_string(),
        ];
        if let Some(kc) = params.keychain {
            codesign_args.push("--keychain".to_string());
            codesign_args.push(kc.to_string());
        }
        if let Some(opts) = params.options
            && !opts.is_empty()
        {
            codesign_args.push("--options".to_string());
            codesign_args.push(opts.join(","));
        }
        if let Some(ent) = params.entitlements {
            codesign_args.push("--entitlements".to_string());
            codesign_args.push(ent.to_string());
        }
        codesign_args.push(bundle_path.to_string());

        log.status(&format!(
            "notarize: signing app bundle {} with codesign",
            bundle.name()
        ));

        if dry_run {
            log.status(&format!(
                "  [dry-run] would run: {}",
                codesign_args.join(" ")
            ));
        } else {
            let status = Command::new(&codesign_args[0])
                .args(&codesign_args[1..])
                .status()
                .with_context(|| {
                    format!("notarize: failed to execute codesign for {}", bundle.name())
                })?;
            if !status.success() {
                bail!(
                    "notarize: codesign failed for {} (exit code: {:?})",
                    bundle.name(),
                    status.code()
                );
            }
        }
    }

    // Step 2: Find DiskImage artifacts for darwin targets and notarize each
    let dmg_artifacts: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| {
            a.kind == ArtifactKind::DiskImage
                && a.target
                    .as_deref()
                    .map(anodize_core::target::is_darwin)
                    .unwrap_or(false)
                && matches_ids(a, params.ids)
        })
        .collect();

    if app_bundles.is_empty() && dmg_artifacts.is_empty() {
        ctx.strict_guard(
            log,
            &format!(
                "notarize: macos_native[{idx}] (dmg) no matching app bundles or DMGs found \
                 (ids={:?})",
                params.ids
            ),
        )?;
        return Ok(());
    }

    // Warn when app bundles were signed but no DMGs found for notarization
    if !app_bundles.is_empty() && dmg_artifacts.is_empty() {
        ctx.strict_guard(
            log,
            &format!("notarize: macos_native[{idx}] signed app bundles but no DMGs found for notarization"),
        )?;
    }

    for dmg in &dmg_artifacts {
        let dmg_path = dmg.path.to_string_lossy();

        // Notarize the DMG
        let mut notarize_args = vec![
            "xcrun".to_string(),
            "notarytool".to_string(),
            "submit".to_string(),
            dmg_path.to_string(),
            "--keychain-profile".to_string(),
            params.profile_name.to_string(),
        ];
        if let Some(kc) = params.keychain {
            notarize_args.push("--keychain".to_string());
            notarize_args.push(kc.to_string());
        }
        if params.wait {
            notarize_args.push("--wait".to_string());
        }
        if let Some(t) = params.timeout {
            notarize_args.push("--timeout".to_string());
            notarize_args.push(t.to_string());
        }

        log.status(&format!(
            "notarize: submitting {} for notarization via xcrun notarytool",
            dmg.name()
        ));

        if dry_run {
            log.status(&format!(
                "  [dry-run] would run: {}",
                notarize_args.join(" ")
            ));
        } else {
            let output = Command::new(&notarize_args[0])
                .args(&notarize_args[1..])
                .output()
                .with_context(|| {
                    format!(
                        "notarize: failed to execute xcrun notarytool for {}",
                        dmg.name()
                    )
                })?;
            check_notarize_output(
                &output,
                &format!("xcrun notarytool submit for {}", dmg.name()),
                log,
            )?;

            // Staple if wait was enabled
            if params.wait {
                let dmg_path_str = dmg_path.to_string();
                let staple_args = ["xcrun", "stapler", "staple", &dmg_path_str];

                log.status(&format!("notarize: stapling {}", dmg.name()));

                let status = Command::new(staple_args[0])
                    .args(&staple_args[1..])
                    .status()
                    .with_context(|| {
                        format!(
                            "notarize: failed to execute xcrun stapler staple for {}",
                            dmg.name()
                        )
                    })?;
                if !status.success() {
                    bail!(
                        "notarize: xcrun stapler staple failed for {} (exit code: {:?})",
                        dmg.name(),
                        status.code()
                    );
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Native PKG mode
// ---------------------------------------------------------------------------

fn run_native_pkg(
    ctx: &Context,
    params: &NativeSignParams,
    dry_run: bool,
    log: &anodize_core::log::StageLogger,
) -> Result<()> {
    let idx = params.idx;

    // Find MacOsPackage artifacts (excluding appbundles) for darwin targets
    let pkg_artifacts: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| {
            a.kind == ArtifactKind::MacOsPackage
                && a.metadata.get("format").map(|f| f.as_str()) != Some("appbundle")
                && a.target
                    .as_deref()
                    .map(anodize_core::target::is_darwin)
                    .unwrap_or(false)
                && matches_ids(a, params.ids)
        })
        .collect();

    if pkg_artifacts.is_empty() {
        ctx.strict_guard(
            log,
            &format!(
                "notarize: macos_native[{idx}] (pkg) no matching PKG artifacts found (ids={:?})",
                params.ids
            ),
        )?;
        return Ok(());
    }

    for pkg in &pkg_artifacts {
        let pkg_path = pkg.path.to_string_lossy();

        // Sign with productsign
        let signed_path = format!("{}.signed", pkg_path);
        let mut sign_args = vec![
            "productsign".to_string(),
            "--sign".to_string(),
            params.identity.to_string(),
        ];
        if let Some(kc) = params.keychain {
            sign_args.push("--keychain".to_string());
            sign_args.push(kc.to_string());
        }
        sign_args.push(pkg_path.to_string());
        sign_args.push(signed_path.clone());

        log.status(&format!(
            "notarize: signing {} with productsign",
            pkg.name()
        ));

        if dry_run {
            log.status(&format!("  [dry-run] would run: {}", sign_args.join(" ")));
        } else {
            let status = Command::new(&sign_args[0])
                .args(&sign_args[1..])
                .status()
                .with_context(|| {
                    format!("notarize: failed to execute productsign for {}", pkg.name())
                })?;
            if !status.success() {
                bail!(
                    "notarize: productsign failed for {} (exit code: {:?})",
                    pkg.name(),
                    status.code()
                );
            }

            // Replace the original with the signed version
            std::fs::rename(&signed_path, pkg_path.as_ref()).with_context(|| {
                format!(
                    "notarize: failed to replace {} with signed version",
                    pkg.name()
                )
            })?;
        }

        // Notarize with xcrun notarytool
        let mut notarize_args = vec![
            "xcrun".to_string(),
            "notarytool".to_string(),
            "submit".to_string(),
            pkg_path.to_string(),
            "--keychain-profile".to_string(),
            params.profile_name.to_string(),
        ];
        if let Some(kc) = params.keychain {
            notarize_args.push("--keychain".to_string());
            notarize_args.push(kc.to_string());
        }
        if params.wait {
            notarize_args.push("--wait".to_string());
        }
        if let Some(t) = params.timeout {
            notarize_args.push("--timeout".to_string());
            notarize_args.push(t.to_string());
        }

        log.status(&format!(
            "notarize: submitting {} for notarization via xcrun notarytool",
            pkg.name()
        ));

        if dry_run {
            log.status(&format!(
                "  [dry-run] would run: {}",
                notarize_args.join(" ")
            ));
        } else {
            let output = Command::new(&notarize_args[0])
                .args(&notarize_args[1..])
                .output()
                .with_context(|| {
                    format!(
                        "notarize: failed to execute xcrun notarytool for {}",
                        pkg.name()
                    )
                })?;
            check_notarize_output(
                &output,
                &format!("xcrun notarytool submit for {}", pkg.name()),
                log,
            )?;

            // Staple if wait was enabled
            if params.wait {
                let pkg_path_str = pkg_path.to_string();
                let staple_args = ["xcrun", "stapler", "staple", &pkg_path_str];

                log.status(&format!("notarize: stapling {}", pkg.name()));

                let status = Command::new(staple_args[0])
                    .args(&staple_args[1..])
                    .status()
                    .with_context(|| {
                        format!(
                            "notarize: failed to execute xcrun stapler staple for {}",
                            pkg.name()
                        )
                    })?;
                if !status.success() {
                    bail!(
                        "notarize: xcrun stapler staple failed for {} (exit code: {:?})",
                        pkg.name(),
                        status.code()
                    );
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use anodize_core::artifact::{Artifact, ArtifactKind};
    use anodize_core::config::{
        Config, MacOSNativeNotarizeConfig, MacOSNativeSignConfig, MacOSNativeSignNotarizeConfig,
        MacOSNotarizeApiConfig, MacOSSignConfig, MacOSSignNotarizeConfig, NotarizeConfig,
        StringOrBool,
    };
    use anodize_core::context::{Context, ContextOptions};

    // -----------------------------------------------------------------------
    // Config deserialization tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_platform_config_deserializes() {
        let yaml = r#"
notarize:
  macos:
    - enabled: true
      ids: [myapp]
      sign:
        certificate: /path/to/cert.p12
        password: "s3cret"
        entitlements: entitlements.xml
      notarize:
        issuer_id: "abc-123"
        key: /path/to/key.p8
        key_id: "KEY123"
        timeout: "15m"
        wait: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        let macos = notarize.macos.unwrap();
        assert_eq!(macos.len(), 1);

        let entry = &macos[0];
        assert_eq!(entry.enabled, Some(StringOrBool::Bool(true)));
        assert_eq!(entry.ids, Some(vec!["myapp".to_string()]));

        let sign = entry.sign.as_ref().unwrap();
        assert_eq!(sign.certificate, Some("/path/to/cert.p12".to_string()));
        assert_eq!(sign.password, Some("s3cret".to_string()));
        assert_eq!(sign.entitlements, Some("entitlements.xml".to_string()));

        let notarize_api = entry.notarize.as_ref().unwrap();
        assert_eq!(notarize_api.issuer_id, Some("abc-123".to_string()));
        assert_eq!(notarize_api.key, Some("/path/to/key.p8".to_string()));
        assert_eq!(notarize_api.key_id, Some("KEY123".to_string()));
        assert_eq!(notarize_api.timeout, Some("15m".to_string()));
        assert_eq!(notarize_api.wait, Some(true));
    }

    #[test]
    fn test_native_config_deserializes() {
        let yaml = r#"
notarize:
  macos_native:
    - enabled: true
      use: dmg
      ids: [myapp]
      sign:
        identity: "Developer ID Application: Example"
        keychain: /path/to/keychain
        options: [runtime]
        entitlements: entitlements.xml
      notarize:
        profile_name: "my-profile"
        wait: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        let native = notarize.macos_native.unwrap();
        assert_eq!(native.len(), 1);

        let entry = &native[0];
        assert_eq!(entry.enabled, Some(StringOrBool::Bool(true)));
        assert_eq!(entry.use_, Some("dmg".to_string()));
        assert_eq!(entry.ids, Some(vec!["myapp".to_string()]));

        let sign = entry.sign.as_ref().unwrap();
        assert_eq!(
            sign.identity,
            Some("Developer ID Application: Example".to_string())
        );
        assert_eq!(sign.keychain, Some("/path/to/keychain".to_string()));
        assert_eq!(sign.options, Some(vec!["runtime".to_string()]));
        assert_eq!(sign.entitlements, Some("entitlements.xml".to_string()));

        let notarize_cfg = entry.notarize.as_ref().unwrap();
        assert_eq!(notarize_cfg.profile_name, Some("my-profile".to_string()));
        assert_eq!(notarize_cfg.wait, Some(true));
    }

    #[test]
    fn test_native_config_pkg_mode_deserializes() {
        let yaml = r#"
notarize:
  macos_native:
    - enabled: true
      use: pkg
      sign:
        identity: "Developer ID Installer: Example"
      notarize:
        profile_name: "my-profile"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        let native = notarize.macos_native.unwrap();
        assert_eq!(native[0].use_, Some("pkg".to_string()));
    }

    #[test]
    fn test_enabled_string_template_deserializes() {
        let yaml = r#"
notarize:
  macos:
    - enabled: "{{ IsCI }}"
      sign:
        certificate: cert.p12
        password: pass
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let macos = config.notarize.unwrap().macos.unwrap();
        match &macos[0].enabled {
            Some(StringOrBool::String(s)) => assert_eq!(s, "{{ IsCI }}"),
            other => panic!("expected StringOrBool::String, got {:?}", other),
        }
    }

    #[test]
    fn test_both_modes_in_single_config() {
        let yaml = r#"
notarize:
  macos:
    - enabled: true
      sign:
        certificate: cert.p12
        password: pass
  macos_native:
    - enabled: true
      sign:
        identity: "Developer ID Application: Test"
      notarize:
        profile_name: test-profile
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        assert!(notarize.macos.is_some());
        assert!(notarize.macos_native.is_some());
    }

    #[test]
    fn test_empty_notarize_config_deserializes() {
        let yaml = r#"
notarize: {}
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let notarize = config.notarize.unwrap();
        assert!(notarize.macos.is_none());
        assert!(notarize.macos_native.is_none());
    }

    // -----------------------------------------------------------------------
    // Stage skipping / enabled logic tests
    // -----------------------------------------------------------------------

    fn make_ctx_with_notarize(config: Config) -> Context {
        Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_stage_skips_when_no_notarize_config() {
        let config = Config::default();
        let mut ctx = make_ctx_with_notarize(config);

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
        // Should succeed with no-op
    }

    #[test]
    fn test_stage_skips_disabled_cross_platform() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(false)),
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
        // Should succeed without errors (disabled)
    }

    #[test]
    fn test_stage_skips_disabled_native() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(false)),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_stage_skips_when_enabled_is_none() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                enabled: None,
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        // Should skip because enabled defaults to false
        stage.run(&mut ctx).unwrap();
    }

    // -----------------------------------------------------------------------
    // Required field validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_platform_requires_sign_config() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                sign: None,
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("requires a 'sign'"),
            "error should mention missing sign config"
        );
    }

    #[test]
    fn test_cross_platform_requires_certificate() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSSignConfig {
                    certificate: None,
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("sign.certificate is required"),
        );
    }

    #[test]
    fn test_cross_platform_requires_password() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: None,
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("sign.password is required"),
        );
    }

    #[test]
    fn test_native_requires_sign_config() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                sign: None,
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("requires a 'sign'"),
        );
    }

    #[test]
    fn test_native_requires_identity() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSNativeSignConfig {
                    identity: None,
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("sign.identity is required"),
        );
    }

    #[test]
    fn test_native_requires_notarize_config() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID".to_string()),
                    ..Default::default()
                }),
                notarize: None,
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("requires a 'notarize'"),
        );
    }

    #[test]
    fn test_native_requires_profile_name() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: None,
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("notarize.profile_name is required"),
        );
    }

    #[test]
    fn test_native_rejects_unsupported_use_type() {
        let mut config = Config::default();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                use_: Some("zip".to_string()),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = make_ctx_with_notarize(config);
        let stage = NotarizeStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unsupported artifact type: zip"),
        );
    }

    // -----------------------------------------------------------------------
    // Dry-run behavior tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_platform_dry_run_with_darwin_binaries() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    entitlements: Some("ent.xml".to_string()),
                }),
                notarize: Some(MacOSNotarizeApiConfig {
                    issuer_id: Some("issuer-123".to_string()),
                    key: Some("key.p8".to_string()),
                    key_id: Some("KEY1".to_string()),
                    wait: Some(true),
                    timeout: Some("20m".to_string()),
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Register darwin binary artifacts
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_x86"),
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        // Also register a linux binary that should be ignored
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_linux"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        // Should succeed without actually invoking rcodesign
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_cross_platform_dry_run_sign_only_no_notarize() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                notarize: None, // sign-only
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_cross_platform_no_darwin_binaries_is_noop() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Only register Linux binaries
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_native_dmg_dry_run_with_artifacts() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                use_: Some("dmg".to_string()),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Application: Test".to_string()),
                    keychain: Some("/path/to/kc".to_string()),
                    options: Some(vec!["runtime".to_string()]),
                    entitlements: Some("ent.xml".to_string()),
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    wait: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Register an app bundle artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.app"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "appbundle".to_string())]),
            size: None,
        });

        // Register a DMG artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DiskImage,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.dmg"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "dmg".to_string())]),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_native_pkg_dry_run_with_artifacts() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                use_: Some("pkg".to_string()),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Installer: Test".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    wait: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Register a MacOsPackage artifact (not appbundle)
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::MacOsPackage,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.pkg"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([
                ("format".to_string(), "pkg".to_string()),
                ("identifier".to_string(), "com.example.myapp".to_string()),
            ]),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_native_dmg_no_matching_artifacts_is_noop() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                use_: Some("dmg".to_string()),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Application: Test".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // No artifacts registered at all
        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_native_pkg_no_matching_artifacts_is_noop() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                use_: Some("pkg".to_string()),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Installer: Test".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
    }

    // -----------------------------------------------------------------------
    // Artifact filtering tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cross_platform_ids_filter() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                ids: Some(vec!["other-crate".to_string()]),
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // This binary is for "myapp" but ids filter is ["other-crate"]
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        // Should succeed with no-op since id doesn't match
        stage.run(&mut ctx).unwrap();
    }

    #[test]
    fn test_matches_ids_helper_no_filter() {
        let artifact = Artifact {
            kind: ArtifactKind::Binary,
            name: "test".to_string(),
            path: PathBuf::from("dist/test"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        };

        assert!(matches_ids(&artifact, &None));
        assert!(matches_ids(&artifact, &Some(vec![])));
    }

    #[test]
    fn test_matches_ids_helper_no_id_metadata_does_not_match() {
        let artifact = Artifact {
            kind: ArtifactKind::Binary,
            name: "test".to_string(),
            path: PathBuf::from("dist/test"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        };

        assert!(!matches_ids(&artifact, &Some(vec!["myapp".to_string()])));
        assert!(!matches_ids(&artifact, &Some(vec!["other".to_string()])));
    }

    #[test]
    fn test_matches_ids_helper_by_metadata_id() {
        let artifact = Artifact {
            kind: ArtifactKind::Binary,
            name: "test".to_string(),
            path: PathBuf::from("dist/test"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-arm".to_string())]),
            size: None,
        };

        assert!(matches_ids(&artifact, &Some(vec!["build-arm".to_string()])));
        assert!(!matches_ids(&artifact, &Some(vec!["myapp".to_string()])));
    }

    #[test]
    fn test_cross_platform_filters_non_darwin_artifacts() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                sign: Some(MacOSSignConfig {
                    certificate: Some("cert.p12".to_string()),
                    password: Some("pass".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Only non-darwin targets
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = NotarizeStage;
        stage.run(&mut ctx).unwrap();
        // No darwin artifacts, so this is a no-op
    }

    #[test]
    fn test_native_dmg_filters_appbundle_by_ids() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                ids: Some(vec!["other".to_string()]),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // This artifact has crate_name "myapp" but ids filter is ["other"]
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.app"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "appbundle".to_string())]),
            size: None,
        });

        let stage = NotarizeStage;
        // Should succeed as no-op since ids don't match
        stage.run(&mut ctx).unwrap();
    }

    // -----------------------------------------------------------------------
    // is_enabled helper tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_enabled_none() {
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        assert!(!is_enabled(&None, &ctx));
    }

    #[test]
    fn test_is_enabled_bool_true() {
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        assert!(is_enabled(&Some(StringOrBool::Bool(true)), &ctx));
    }

    #[test]
    fn test_is_enabled_bool_false() {
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        assert!(!is_enabled(&Some(StringOrBool::Bool(false)), &ctx));
    }

    #[test]
    fn test_is_enabled_string_true() {
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        assert!(is_enabled(
            &Some(StringOrBool::String("true".to_string())),
            &ctx
        ));
    }

    #[test]
    fn test_is_enabled_string_false() {
        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());
        assert!(!is_enabled(
            &Some(StringOrBool::String("false".to_string())),
            &ctx
        ));
    }

    // -----------------------------------------------------------------------
    // Native DMG mode defaults to "dmg" when use_ is None
    // -----------------------------------------------------------------------

    #[test]
    fn test_native_defaults_to_dmg_when_use_is_none() {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            disable: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                enabled: Some(StringOrBool::Bool(true)),
                use_: None, // should default to "dmg"
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Application: Test".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        // Register a DMG so the stage has something to find (or not)
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DiskImage,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.dmg"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "dmg".to_string())]),
            size: None,
        });

        let stage = NotarizeStage;
        // Should succeed because it defaults to DMG mode
        stage.run(&mut ctx).unwrap();
    }
}
