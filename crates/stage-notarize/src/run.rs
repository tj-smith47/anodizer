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

#[cfg(all(test, unix))]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use std::collections::HashMap;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::Output;

    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        Config, MacOSNativeArtifactKind, MacOSNativeNotarizeConfig, MacOSNativeSignConfig,
        MacOSNativeSignNotarizeConfig, MacOSNotarizeApiConfig, MacOSSignConfig,
        MacOSSignNotarizeConfig, NotarizeConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::stage::Stage;
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use tempfile::TempDir;

    use super::{check_notarize_output, run_with_retry};
    use crate::NotarizeStage;

    fn logger() -> StageLogger {
        StageLogger::new("notarize", Verbosity::Quiet)
    }

    /// Fabricate an [`Output`] with a chosen exit code and combined output. A
    /// raw status of `code << 8` is how the kernel encodes a normal exit, so
    /// `ExitStatusExt::from_raw` yields a status whose `.code()` is `code`.
    fn output_with(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: ExitStatusExt::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// Drop a real file at `path` so the stage's stat-checks and in-place
    /// rename/checksum-refresh operate on something concrete.
    fn touch(dir: &TempDir, name: &str, contents: &str) -> PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }

    // -----------------------------------------------------------------------
    // check_notarize_output: status classification branches
    // -----------------------------------------------------------------------

    #[test]
    fn check_output_accepted_success_is_ok() {
        let out = output_with(
            0,
            "{\"status\":\"Accepted\"}\nProcessing complete\n  status: Accepted\n",
            "",
        );
        check_notarize_output(&out, "submit for app", &logger()).unwrap();
    }

    #[test]
    fn check_output_success_with_timeout_warns_but_ok() {
        // Zero exit but the body mentions a timeout: non-fatal, returns Ok.
        let out = output_with(0, "submission still processing: timeout reached\n", "");
        check_notarize_output(&out, "submit for app", &logger()).unwrap();
    }

    #[test]
    fn check_output_invalid_status_bails() {
        let out = output_with(1, "  status: Invalid\nThe binary is not signed\n", "");
        let err = check_notarize_output(&out, "submit for app", &logger()).unwrap_err();
        assert!(
            err.to_string().contains("invalid"),
            "expected invalid classification, got: {err}"
        );
    }

    #[test]
    fn check_output_invalid_submission_phrase_bails() {
        let out = output_with(1, "", "error: invalid submission rejected by checks\n");
        let err = check_notarize_output(&out, "submit for app", &logger()).unwrap_err();
        assert!(err.to_string().contains("invalid"));
    }

    #[test]
    fn check_output_rejected_status_bails() {
        let out = output_with(1, "  status: Rejected\n", "");
        let err = check_notarize_output(&out, "submit for app", &logger()).unwrap_err();
        assert!(
            err.to_string().contains("rejected"),
            "expected rejected classification, got: {err}"
        );
    }

    #[test]
    fn check_output_timeout_on_failure_is_nonfatal_ok() {
        // Non-zero exit but the only signal is a timeout -> treated as Ok.
        let out = output_with(1, "", "operation timed out waiting for Apple\n");
        check_notarize_output(&out, "submit for app", &logger()).unwrap();
    }

    #[test]
    fn check_output_generic_failure_bails_with_exit_code() {
        let out = output_with(7, "", "xcrun: error: unrecognized flag --bogus\n");
        let err = check_notarize_output(&out, "submit for app", &logger()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("failed (exit code"), "got: {msg}");
        assert!(
            msg.contains("unrecognized flag"),
            "should surface stderr: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // run_with_retry: transient failure then success drives >1 invocation
    // -----------------------------------------------------------------------

    #[test]
    #[serial_test::serial]
    fn retry_driver_reinvokes_after_transient_failure() {
        let tools = FakeToolDir::new();
        // First call: exit 1 with a retriable network marker. Second call:
        // exit 0. The stub flips on a marker file it creates after attempt 1.
        tools
            .tool("rcodesign")
            .script(
                "if [ -f .attempted ]; then exit 0; fi\n\
                 touch .attempted\n\
                 echo 'error: connection reset by peer' 1>&2\n\
                 exit 1",
            )
            .install();
        let work = TempDir::new().unwrap();
        let bin = tools.tool_path("rcodesign").to_string_lossy().to_string();
        let args = vec![bin, "notary-submit".to_string()];
        // No-op sleeper so the exponential backoff doesn't stall the suite.
        let nap = |_: std::time::Duration| {};
        // Run from `work` so the `.attempted` marker lands in a temp dir.
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(work.path()).unwrap();
        let res = run_with_retry(&args, "rcodesign notary-submit", &logger(), &nap);
        std::env::set_current_dir(prev).unwrap();

        let out = res.unwrap();
        assert!(out.status.success(), "second attempt should succeed");
        assert!(
            tools.call_count("rcodesign") >= 2,
            "transient failure must trigger a re-invocation; got {} calls",
            tools.call_count("rcodesign")
        );
    }

    #[test]
    #[serial_test::serial]
    fn retry_driver_does_not_retry_nonretriable_failure() {
        let tools = FakeToolDir::new();
        // status: Invalid is a hard Apple rejection — must NOT retry.
        tools
            .tool("rcodesign")
            .stdout("  status: Invalid\n")
            .exit(1)
            .install();
        let bin = tools.tool_path("rcodesign").to_string_lossy().to_string();
        let args = vec![bin, "notary-submit".to_string()];
        let nap = |_: std::time::Duration| {};
        let out = run_with_retry(&args, "rcodesign notary-submit", &logger(), &nap).unwrap();
        assert!(!out.status.success());
        assert_eq!(
            tools.call_count("rcodesign"),
            1,
            "non-retriable failure must run exactly once"
        );
    }

    // -----------------------------------------------------------------------
    // Native DMG end-to-end: codesign + xcrun notarytool + xcrun stapler
    // -----------------------------------------------------------------------

    fn native_dmg_config(wait: bool, opts: Option<Vec<String>>) -> Config {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                use_: Some(MacOSNativeArtifactKind::Dmg),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Application: Test".to_string()),
                    keychain: Some("/path/to/kc".to_string()),
                    options: opts,
                    entitlements: Some("ent.xml".to_string()),
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    wait: Some(wait),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });
        config
    }

    fn dmg_ctx(config: Config, work: &TempDir) -> Context {
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        let bundle = touch(work, "MyApp.app", "bundle");
        let dmg = touch(work, "MyApp.dmg", "diskimage-bytes");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: String::new(),
            path: bundle,
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([
                ("format".to_string(), "appbundle".to_string()),
                ("id".to_string(), "myapp".to_string()),
            ]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DiskImage,
            name: String::new(),
            path: dmg,
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([
                ("format".to_string(), "dmg".to_string()),
                ("id".to_string(), "myapp".to_string()),
            ]),
            size: None,
        });
        ctx
    }

    #[test]
    #[serial_test::serial]
    fn native_dmg_signs_notarizes_and_staples() {
        let tools = FakeToolDir::new();
        tools.tool("codesign").install();
        tools.tool("xcrun").stdout("  status: Accepted\n").install();
        let work = TempDir::new().unwrap();
        let mut ctx = dmg_ctx(
            native_dmg_config(true, Some(vec!["runtime".to_string()])),
            &work,
        );

        let _g = tools.activate();
        NotarizeStage.run(&mut ctx).unwrap();
        drop(_g);

        // codesign: --deep --force --sign <identity> ... --keychain ... --options runtime --entitlements ent.xml <bundle>
        let cs = tools.calls("codesign");
        assert_eq!(cs.len(), 1, "one app bundle signed");
        let argv = &cs[0];
        assert_eq!(argv[0], "--deep");
        assert_eq!(argv[1], "--force");
        assert_eq!(argv[2], "--sign");
        assert_eq!(argv[3], "Developer ID Application: Test");
        assert!(argv.iter().any(|a| a == "--keychain"));
        let opt_idx = argv
            .iter()
            .position(|a| a == "--options")
            .expect("--options present");
        assert_eq!(argv[opt_idx + 1], "runtime");
        let ent_idx = argv
            .iter()
            .position(|a| a == "--entitlements")
            .expect("--entitlements present");
        assert_eq!(argv[ent_idx + 1], "ent.xml");
        assert!(argv.last().unwrap().ends_with("MyApp.app"));

        // xcrun was invoked twice: notarytool submit, then stapler staple.
        let xc = tools.calls("xcrun");
        assert_eq!(xc.len(), 2, "submit + staple");
        assert_eq!(xc[0][0], "notarytool");
        assert_eq!(xc[0][1], "submit");
        assert!(xc[0].iter().any(|a| a == "--keychain-profile"));
        assert!(xc[0].iter().any(|a| a == "--wait"), "wait:true adds --wait");
        assert_eq!(xc[1][0], "stapler");
        assert_eq!(xc[1][1], "staple");
        assert!(xc[1][2].ends_with("MyApp.dmg"));
    }

    #[test]
    #[serial_test::serial]
    fn native_dmg_wait_false_skips_stapling() {
        let tools = FakeToolDir::new();
        tools.tool("codesign").install();
        tools.tool("xcrun").stdout("  status: Accepted\n").install();
        let work = TempDir::new().unwrap();
        let mut ctx = dmg_ctx(native_dmg_config(false, None), &work);

        let _g = tools.activate();
        NotarizeStage.run(&mut ctx).unwrap();
        drop(_g);

        let xc = tools.calls("xcrun");
        assert_eq!(xc.len(), 1, "only submit, no staple when wait:false");
        assert_eq!(xc[0][0], "notarytool");
        assert!(
            !xc[0].iter().any(|a| a == "--wait"),
            "wait:false omits --wait"
        );
    }

    #[test]
    #[serial_test::serial]
    fn native_dmg_codesign_nonzero_exit_errors() {
        let tools = FakeToolDir::new();
        tools
            .tool("codesign")
            .stderr("codesign: errSecInternalComponent\n")
            .exit(1)
            .install();
        tools.tool("xcrun").stdout("  status: Accepted\n").install();
        let work = TempDir::new().unwrap();
        let mut ctx = dmg_ctx(native_dmg_config(true, None), &work);

        let _g = tools.activate();
        let err = NotarizeStage.run(&mut ctx).unwrap_err();
        drop(_g);

        assert!(err.to_string().contains("codesign failed"), "got: {err}");
        // notarytool must not run after a codesign failure.
        assert!(
            !tools.was_called("xcrun"),
            "xcrun should not run after codesign fails"
        );
    }

    #[test]
    #[serial_test::serial]
    fn native_dmg_notarytool_invalid_errors_before_staple() {
        let tools = FakeToolDir::new();
        tools.tool("codesign").install();
        tools
            .tool("xcrun")
            .stdout("  status: Invalid\nartifact failed Apple checks\n")
            .exit(1)
            .install();
        let work = TempDir::new().unwrap();
        let mut ctx = dmg_ctx(native_dmg_config(true, None), &work);

        let _g = tools.activate();
        let err = NotarizeStage.run(&mut ctx).unwrap_err();
        drop(_g);

        assert!(err.to_string().contains("invalid"), "got: {err}");
        // Exactly one xcrun call (the submit); stapler is never reached.
        let xc = tools.calls("xcrun");
        assert_eq!(
            xc.len(),
            1,
            "stapler must not run after an invalid notarization"
        );
        assert_eq!(xc[0][0], "notarytool");
    }

    #[test]
    #[serial_test::serial]
    fn native_dmg_stapler_nonzero_exit_errors() {
        let tools = FakeToolDir::new();
        tools.tool("codesign").install();
        // notarytool accepts, stapler fails.
        tools
            .tool("xcrun")
            .script(
                "case \"$1\" in\n\
                 notarytool) printf '  status: Accepted\\n'; exit 0;;\n\
                 stapler) printf 'CloudKit query failed\\n' 1>&2; exit 65;;\n\
                 esac",
            )
            .install();
        let work = TempDir::new().unwrap();
        let mut ctx = dmg_ctx(native_dmg_config(true, None), &work);

        let _g = tools.activate();
        let err = NotarizeStage.run(&mut ctx).unwrap_err();
        drop(_g);

        assert!(
            err.to_string().contains("stapler staple failed"),
            "got: {err}"
        );
        assert_eq!(tools.call_count("xcrun"), 2, "submit then staple both ran");
    }

    // -----------------------------------------------------------------------
    // Native PKG end-to-end: productsign + xcrun notarytool (+ rename)
    // -----------------------------------------------------------------------

    fn native_pkg_config(wait: bool) -> Config {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: None,
            macos_native: Some(vec![MacOSNativeSignNotarizeConfig {
                skip: None,
                use_: Some(MacOSNativeArtifactKind::Pkg),
                sign: Some(MacOSNativeSignConfig {
                    identity: Some("Developer ID Installer: Test".to_string()),
                    keychain: Some("/path/to/kc".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNativeNotarizeConfig {
                    profile_name: Some("my-profile".to_string()),
                    wait: Some(wait),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
        });
        config
    }

    fn pkg_ctx(config: Config, work: &TempDir) -> Context {
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        let pkg = touch(work, "MyApp.pkg", "pkg-bytes");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::MacOsPackage,
            name: String::new(),
            path: pkg,
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([
                ("format".to_string(), "pkg".to_string()),
                ("id".to_string(), "myapp".to_string()),
            ]),
            size: None,
        });
        ctx
    }

    #[test]
    #[serial_test::serial]
    fn native_pkg_productsigns_and_notarizes() {
        let tools = FakeToolDir::new();
        // productsign writes the `<pkg>.signed` output the stage renames over the original.
        tools
            .tool("productsign")
            .script("for dest in \"$@\"; do :; done; printf 'signed-bytes' > \"$dest\"; exit 0")
            .install();
        tools.tool("xcrun").stdout("  status: Accepted\n").install();
        let work = TempDir::new().unwrap();
        let pkg_path = work.path().join("MyApp.pkg");
        let mut ctx = pkg_ctx(native_pkg_config(true), &work);

        let _g = tools.activate();
        NotarizeStage.run(&mut ctx).unwrap();
        drop(_g);

        // productsign: --sign <identity> --keychain <kc> <pkg> <pkg>.signed
        let ps = tools.calls("productsign");
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0][0], "--sign");
        assert_eq!(ps[0][1], "Developer ID Installer: Test");
        assert!(ps[0].iter().any(|a| a == "--keychain"));
        assert!(ps[0][ps[0].len() - 1].ends_with("MyApp.pkg.signed"));
        // The signed file was renamed over the original.
        assert_eq!(std::fs::read_to_string(&pkg_path).unwrap(), "signed-bytes");
        assert!(
            !work.path().join("MyApp.pkg.signed").exists(),
            "renamed away"
        );

        // notarytool submit + stapler staple (wait:true).
        let xc = tools.calls("xcrun");
        assert_eq!(xc.len(), 2);
        assert_eq!(xc[0][0], "notarytool");
        assert_eq!(xc[0][1], "submit");
        assert_eq!(xc[1][0], "stapler");
    }

    #[test]
    #[serial_test::serial]
    fn native_pkg_productsign_nonzero_exit_errors() {
        let tools = FakeToolDir::new();
        tools
            .tool("productsign")
            .stderr("productsign: no identity found\n")
            .exit(1)
            .install();
        tools.tool("xcrun").stdout("  status: Accepted\n").install();
        let work = TempDir::new().unwrap();
        let mut ctx = pkg_ctx(native_pkg_config(true), &work);

        let _g = tools.activate();
        let err = NotarizeStage.run(&mut ctx).unwrap_err();
        drop(_g);

        assert!(err.to_string().contains("productsign failed"), "got: {err}");
        assert!(
            !tools.was_called("xcrun"),
            "notarytool must not run after sign failure"
        );
    }

    // -----------------------------------------------------------------------
    // Cross-platform (rcodesign) end-to-end: sign + notary-submit
    // -----------------------------------------------------------------------

    #[test]
    #[serial_test::serial]
    fn cross_platform_signs_and_submits_with_rcodesign() {
        let tools = FakeToolDir::new();
        // One rcodesign stub handles both `sign` and `notary-submit`; emit an
        // Accepted line so check_notarize_output classifies the submit cleanly.
        tools
            .tool("rcodesign")
            .stdout("notarization: Accepted\n")
            .install();
        let work = TempDir::new().unwrap();
        let cert = touch(&work, "cert.p12", "p12-bytes");
        let key = touch(&work, "key.p8", "p8-bytes");
        let bin = touch(&work, "myapp", "mach-o");

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSSignConfig {
                    certificate: Some(cert.to_string_lossy().to_string()),
                    password: Some("s3cret".to_string()),
                    ..Default::default()
                }),
                notarize: Some(MacOSNotarizeApiConfig {
                    issuer_id: Some("issuer-123".to_string()),
                    key: Some(key.to_string_lossy().to_string()),
                    key_id: Some("KEY1".to_string()),
                    wait: Some(true),
                    timeout: Some(anodizer_core::config::HumanDuration(
                        std::time::Duration::from_secs(15 * 60),
                    )),
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin,
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "myapp".to_string())]),
            size: None,
        });

        let _g = tools.activate();
        NotarizeStage.run(&mut ctx).unwrap();
        drop(_g);

        let calls = tools.calls("rcodesign");
        assert_eq!(calls.len(), 2, "sign then notary-submit");
        // First call: sign with --p12-file <cert> --p12-password <pw> --timestamp-url <url> <bin>
        assert_eq!(calls[0][0], "sign");
        let p12_idx = calls[0]
            .iter()
            .position(|a| a == "--p12-file")
            .expect("--p12-file");
        assert!(calls[0][p12_idx + 1].ends_with("cert.p12"));
        assert!(calls[0].iter().any(|a| a == "--p12-password"));
        assert!(calls[0].iter().any(|a| a == "--timestamp-url"));
        assert!(calls[0].last().unwrap().ends_with("myapp"));
        // Second call: notary-submit --api-issuer ... --api-key KEY1 --api-key-path <key> --wait --max-wait 15m <bin>
        assert_eq!(calls[1][0], "notary-submit");
        let iss_idx = calls[1]
            .iter()
            .position(|a| a == "--api-issuer")
            .expect("--api-issuer");
        assert_eq!(calls[1][iss_idx + 1], "issuer-123");
        let key_idx = calls[1]
            .iter()
            .position(|a| a == "--api-key")
            .expect("--api-key");
        assert_eq!(calls[1][key_idx + 1], "KEY1");
        assert!(calls[1].iter().any(|a| a == "--wait"));
        let mw_idx = calls[1]
            .iter()
            .position(|a| a == "--max-wait")
            .expect("--max-wait");
        assert_eq!(calls[1][mw_idx + 1], "15m");
    }

    #[test]
    #[serial_test::serial]
    fn cross_platform_missing_certificate_path_errors() {
        // Non-dry-run stat-check rejects a certificate path that does not exist
        // before any rcodesign spawn.
        let tools = FakeToolDir::new();
        tools.tool("rcodesign").install();
        let work = TempDir::new().unwrap();
        let bin = touch(&work, "myapp", "mach-o");

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.notarize = Some(NotarizeConfig {
            skip: None,
            macos: Some(vec![MacOSSignNotarizeConfig {
                skip: None,
                sign: Some(MacOSSignConfig {
                    certificate: Some("/no/such/cert.p12".to_string()),
                    password: Some("pw".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            macos_native: None,
        });

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin,
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "myapp".to_string())]),
            size: None,
        });

        let _g = tools.activate();
        let err = NotarizeStage.run(&mut ctx).unwrap_err();
        drop(_g);

        assert!(
            err.to_string()
                .contains("sign.certificate path does not exist"),
            "got: {err}"
        );
        assert!(
            !tools.was_called("rcodesign"),
            "rcodesign must not spawn on a bad cert path"
        );
    }
}
