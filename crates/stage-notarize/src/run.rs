//! The per-config signing/notarization run paths: cross-platform
//! (rcodesign) and native (codesign + xcrun notarytool), including the
//! DMG and PKG native variants.

use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{MacOSNativeSignNotarizeConfig, MacOSSignNotarizeConfig};
use anodizer_core::context::Context;

use super::retry::{check_notarize_output, real_sleep, run_status_with_retry, run_with_retry};
use super::secret::{matches_ids, materialize_secret, redact_args};

// ---------------------------------------------------------------------------
// Cross-platform (rcodesign)
// ---------------------------------------------------------------------------

pub(super) fn run_cross_platform(
    ctx: &Context,
    cfg: &MacOSSignNotarizeConfig,
    idx: usize,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    if cfg
        .should_skip(|s| ctx.render_template(s))
        .with_context(|| format!("notarize: macos[{idx}] evaluate skip expression"))?
    {
        log.status(&format!("notarize: macos[{idx}] skipped (skip: true)"));
        return Ok(());
    }

    // Validate and render sign config
    let sign = cfg
        .sign
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("notarize: macos[{idx}] requires a 'sign' configuration"))?;

    let certificate_raw = ctx
        .render_template_opt(sign.certificate.as_deref())
        .with_context(|| format!("notarize: macos[{idx}] render sign.certificate"))?
        .ok_or_else(|| anyhow::anyhow!("notarize: macos[{idx}] sign.certificate is required"))?;

    // `certificate:` may be either a path OR a base64-encoded
    // P12 blob (the latter is the common shape for storing the cert in a
    // CI secret store). Materialize the base64 form to a tempfile so
    // rcodesign can read it via its `--p12-file` flag.
    // Held for its Drop: the binding owns the materialized base64-decoded
    // cert tempfile, which must outlive the rcodesign subprocess that reads
    // it. Dropping early would delete the file before the spawn below.
    let cert_secret = materialize_secret(&certificate_raw, "sign.certificate")
        .with_context(|| format!("notarize: macos[{idx}] materialize sign.certificate"))?;
    let certificate = cert_secret.path.clone();

    // Stat-check the resolved path before launching rcodesign so a typo or
    // missing file produces a clean "certificate not found" error instead
    // of an opaque rcodesign exit code partway through artifact
    // iteration. Skipped in dry-run because dry-run never actually
    // invokes rcodesign and upstream callers (incl. tests) commonly
    // point to placeholder paths.
    if !dry_run && !std::path::Path::new(&certificate).exists() {
        bail!("notarize: macos[{idx}] sign.certificate path does not exist: '{certificate}'");
    }

    let password = ctx
        .render_template_opt(sign.password.as_deref())
        .with_context(|| format!("notarize: macos[{idx}] render sign.password"))?
        .ok_or_else(|| anyhow::anyhow!("notarize: macos[{idx}] sign.password is required"))?;

    let entitlements = ctx
        .render_template_opt(sign.entitlements.as_deref())
        .with_context(|| format!("notarize: macos[{idx}] render sign.entitlements"))?;

    // Render and validate notarize config fields (if present). The block
    // yields both the API parameters and the materialized .p8 key guard;
    // `key_secret` below is held (never read) purely so its Drop — which
    // deletes the base64-decoded key tempfile — is deferred until the
    // notary-submit subprocess has launched. The name is kept (not
    // underscore-prefixed) so it reads as load-bearing rather than
    // discardable; the lint allow is the only honest way to say "held only
    // for its Drop" without the misleading `_` prefix.
    #[allow(unused_variables)]
    let (notarize_api, key_secret) = if let Some(ref ncfg) = cfg.notarize {
        let issuer_id = ctx.render_template_opt(ncfg.issuer_id.as_deref())
            .with_context(|| format!("notarize: macos[{idx}] render notarize.issuer_id"))?
            .ok_or_else(|| {
                anyhow::anyhow!("notarize: macos[{idx}] notarize.issuer_id is required when notarize block is present")
            })?;
        let key_raw = ctx
            .render_template_opt(ncfg.key.as_deref())
            .with_context(|| format!("notarize: macos[{idx}] render notarize.key"))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "notarize: macos[{idx}] notarize.key is required when notarize block is present"
                )
            })?;
        // Same path-or-base64 contract as the certificate above. The
        // .p8 API key is commonly stored as `APPLE_API_KEY=$(cat key.p8 | base64)`
        // in CI; materialize the base64 form to a tempfile that survives
        // until the end of run_cross_platform.
        let secret = materialize_secret(&key_raw, "notarize.key")
            .with_context(|| format!("notarize: macos[{idx}] materialize notarize.key"))?;
        let key = secret.path.clone();
        // Stat-check the resolved path before launching rcodesign so a
        // typo or unmounted secret produces a clean "key not found"
        // error instead of an opaque rcodesign exit code partway through
        // artifact iteration. Skipped in dry-run for the same reason as
        // the cert check above.
        if !dry_run && !std::path::Path::new(&key).exists() {
            bail!("notarize: macos[{idx}] notarize.key path does not exist: '{key}'");
        }
        let key_id = ctx.render_template_opt(ncfg.key_id.as_deref())
            .with_context(|| format!("notarize: macos[{idx}] render notarize.key_id"))?
            .ok_or_else(|| {
                anyhow::anyhow!("notarize: macos[{idx}] notarize.key_id is required when notarize block is present")
            })?;
        let timeout = Some(ncfg.resolved_timeout());
        (
            Some((issuer_id, key, key_id, ncfg.resolved_wait(), timeout)),
            Some(secret),
        )
    } else {
        (None, None)
    };

    // Default IDs to project name when not specified
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
                    .map(anodizer_core::target::is_darwin)
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

        // Resolve the timestamp URL once per artifact: per-config override
        // wins over the Apple default.
        let timestamp_url = sign.resolved_timestamp_url();

        let mut sign_args = vec![
            "rcodesign".to_string(),
            "sign".to_string(),
            "--p12-file".to_string(),
            certificate.clone(),
            "--p12-password".to_string(),
            password.clone(),
            "--timestamp-url".to_string(),
            timestamp_url.to_string(),
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
                redact_args(&sign_args, log).join(" ")
            ));
        } else {
            // M6: rcodesign sign contacts Apple's RFC 3161 timestamp server
            // (`http://timestamp.apple.com/ts01`); transient blips there used
            // to fail the whole release. Wrap in the 3-attempt 30s
            // exponential retry.
            let label = format!("rcodesign sign for {}", artifact.name());
            let status = run_status_with_retry(&sign_args, &label, log, &real_sleep)?;
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
                    redact_args(&notarize_args, log).join(" ")
                ));
            } else {
                // M6: wrap in a 3-attempt 30s exponential retry so a
                // transient blip on the App Store Connect API does not fail
                // the whole release (notary-submit talks directly to
                // Apple-hosted services).
                let label = format!("rcodesign notary-submit for {}", artifact.name());
                let output = run_with_retry(&notarize_args, &label, log, &real_sleep)?;
                check_notarize_output(&output, &label, log)?;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Native (codesign + xcrun notarytool)
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

pub(super) fn run_native(
    ctx: &Context,
    cfg: &MacOSNativeSignNotarizeConfig,
    idx: usize,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    if cfg
        .should_skip(|s| ctx.render_template(s))
        .with_context(|| format!("notarize: macos_native[{idx}] evaluate skip expression"))?
    {
        log.status(&format!(
            "notarize: macos_native[{idx}] skipped (skip: true)"
        ));
        return Ok(());
    }

    use anodizer_core::config::MacOSNativeArtifactKind;
    let artifact_type = cfg.resolved_use();

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

    let wait = notarize.resolved_wait();

    let timeout = Some(notarize.resolved_timeout());

    // Default IDs to project name when not specified
    let ids = cfg.ids.clone().or_else(|| {
        if ctx.config.project_name.is_empty() {
            None
        } else {
            Some(vec![ctx.config.project_name.clone()])
        }
    });

    // Issue 9: Warn if options set with use: pkg (options only apply to DMGs)
    if artifact_type == MacOSNativeArtifactKind::Pkg
        && sign.options.as_ref().is_some_and(|o| !o.is_empty())
    {
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
        MacOSNativeArtifactKind::Dmg => run_native_dmg(ctx, &params, dry_run, log),
        MacOSNativeArtifactKind::Pkg => run_native_pkg(ctx, &params, dry_run, log),
    }
}

// ---------------------------------------------------------------------------
// Native DMG mode
// ---------------------------------------------------------------------------

fn run_native_dmg(
    ctx: &Context,
    params: &NativeSignParams,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let idx = params.idx;

    // Find AppBundle (Installer with format=appbundle) artifacts for darwin targets
    let app_bundles: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| {
            a.kind == ArtifactKind::Installer
                && a.metadata.get("format").map(|f| f.as_str()) == Some("appbundle")
                && a.target
                    .as_deref()
                    .map(anodizer_core::target::is_darwin)
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

    // Find DiskImage artifacts for darwin targets and notarize each
    let dmg_artifacts: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| {
            a.kind == ArtifactKind::DiskImage
                && a.target
                    .as_deref()
                    .map(anodizer_core::target::is_darwin)
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
            // M6: wrap notarytool submit in a 3-attempt 30s exponential
            // retry; the call talks directly to Apple-hosted services and a
            // transient blip should not fail the whole release.
            let label = format!("xcrun notarytool submit for {}", dmg.name());
            let output = run_with_retry(&notarize_args, &label, log, &real_sleep)?;
            check_notarize_output(&output, &label, log)?;

            // Staple if wait was enabled. Without `wait: true`, the
            // submit returns before Apple completes processing, so the
            // ticket isn't available to staple. Surface that explicitly
            // so a user who expected a stapled DMG knows the publisher
            // skipped that step on purpose.
            if !params.wait {
                log.status(&format!(
                    "notarize: {} submitted (wait disabled; ticket will not be stapled — \
                     end-users will need an internet connection on first launch)",
                    dmg.name()
                ));
            }
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
    log: &anodizer_core::log::StageLogger,
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
                    .map(anodizer_core::target::is_darwin)
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
            // M6: 3-attempt 30s exponential retry around the Apple-hosted
            // notarytool submit call.
            let label = format!("xcrun notarytool submit for {}", pkg.name());
            let output = run_with_retry(&notarize_args, &label, log, &real_sleep)?;
            check_notarize_output(&output, &label, log)?;

            // Without `wait: true`, the submit returns before Apple
            // completes processing, so the ticket isn't available to
            // staple. Surface that explicitly so a user who expected a
            // stapled PKG knows the publisher skipped that step on
            // purpose.
            if !params.wait {
                log.status(&format!(
                    "notarize: {} submitted (wait disabled; ticket will not be stapled — \
                     end-users will need an internet connection on first launch)",
                    pkg.name()
                ));
            }
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
