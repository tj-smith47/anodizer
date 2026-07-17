//! Standalone cryptographic re-verification of already-produced signature
//! assets, consumed by the post-publish gate (`verify-release`).
//!
//! The sign stage verifies each signature the moment it is produced
//! ([`crate::verify`]); this module re-runs the SAME derived verification at
//! release-verification time. When the caller supplies the PUBLISHED
//! signature/certificate bytes ([`PublishedSignatureSource`], downloaded
//! from the live release), those bytes are verified against the local
//! payload — whose equality with the published payload the caller's digest
//! check establishes — so a signature that was corrupted, replaced, or
//! forged ON THE RELEASE is caught. When the published bytes are not
//! available (no download, network failure), the locally-produced bytes are
//! verified instead: still proof the signing material and payload agree,
//! but on-release tampering of the signature itself is then covered only by
//! the caller's presence + non-empty check. Everything else is derived from
//! the resolved `signs:` config exactly as the sign stage derives it — the
//! signer `cmd`, its argv (whose `--key` presence decides keyed vs keyless
//! cosign), the per-artifact signature/certificate output paths, and the
//! rendered `env:` — so no additional configuration exists to drift.
//!
//! ## Failure semantics (load-bearing)
//!
//! Only a signature the verifier POSITIVELY rejected reports as
//! [`SignatureCryptoOutcome::Invalid`]. Every environmental shortfall — the
//! verifier binary absent, key material not loadable, keyless identity not
//! derivable, transparency log unreachable — yields NO outcome for the
//! asset (a verbose notice instead), and the consumer falls back to its
//! presence + non-empty check. A release that verifies clean today can
//! therefore never newly fail because the verify environment is poorer than
//! the sign environment.

use std::collections::BTreeMap;

use anodizer_core::config::SignConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

use crate::expected::expected_output_paths;
use crate::helpers::{default_sign_cmd, should_sign_artifact, sign_ids_match};
use crate::process::ensure_cosign_consent_env;
use crate::verify::{
    ConfigVerifyMode, VerifyJob, VerifyRunVerdict, build_blob_verify_args,
    derive_cosign_public_key, execute_verify_job_classified, resolve_config_verify_mode,
};

/// Cryptographic verdict for one signature / certificate asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureCryptoOutcome {
    /// The signature verified against its payload with the derived material.
    Verified,
    /// The verifier positively rejected the signature; carries the
    /// verifier's diagnostic.
    Invalid(String),
}

/// Per-asset cryptographic verdicts for one crate's signature assets, keyed
/// by the name the asset is published under (its uploaded name when the
/// upload renamed it, otherwise its basename). An asset with no entry could
/// not be checked (environmental shortfall, see the module docs) and must
/// fall back to the presence-only check.
#[derive(Debug, Default)]
pub struct SignatureVerification {
    outcomes: BTreeMap<String, SignatureCryptoOutcome>,
}

impl SignatureVerification {
    /// The verdict for `asset_name`, when the asset was checkable.
    pub fn outcome(&self, asset_name: &str) -> Option<&SignatureCryptoOutcome> {
        self.outcomes.get(asset_name)
    }

    /// Record a verdict. A positive verification by ANY producing config
    /// wins over a rejection by another: when two sign configs name the
    /// same output path, the on-disk bytes are the last writer's, so the
    /// earlier config's verifier legitimately rejects bytes it never wrote —
    /// while one positive verify under trusted material proves the asset.
    fn record(&mut self, name: String, outcome: SignatureCryptoOutcome) {
        match self.outcomes.get(&name) {
            Some(SignatureCryptoOutcome::Verified) => {}
            _ => {
                self.outcomes.insert(name, outcome);
            }
        }
    }
}

/// Caller-supplied view of the PUBLISHED signature assets. The caller (the
/// verify-release gate) owns release/asset access — URLs, tokens, the
/// download itself — while the cryptography stays here; this struct is the
/// boundary between the two.
///
/// Both maps may be empty ([`Default`]): verification then runs against the
/// locally-produced bytes under their basenames, which is also the fallback
/// for any individual asset missing from `downloaded`.
#[derive(Debug, Default)]
pub struct PublishedSignatureSource {
    /// Local signature/certificate output path → the asset name it was
    /// uploaded under. Entries are needed only for uploads that renamed the
    /// file; an absent entry means the basename was used.
    pub uploaded_names: BTreeMap<std::path::PathBuf, String>,
    /// Uploaded asset name → local temp file holding the downloaded
    /// PUBLISHED bytes for that asset.
    pub downloaded: BTreeMap<String, std::path::PathBuf>,
}

impl PublishedSignatureSource {
    /// The name `local_path`'s asset is published under.
    fn published_name(&self, local_path: &std::path::Path) -> String {
        self.uploaded_names
            .get(local_path)
            .cloned()
            .unwrap_or_else(|| basename(local_path))
    }
}

/// Re-verify the on-disk signature / certificate assets the resolved
/// `signs:` config produced for `crate_name`, using verification material
/// derived from that same config (keyed cosign public key via
/// `cosign public-key`, keyless identity from config or the ambient GitHub
/// Actions OIDC environment, gpg against the signing keyring).
///
/// `release_ids` is the release block's `ids:` upload filter, applied the
/// same way [`crate::expected_signature_assets`] applies it, so only
/// signatures whose subject is uploaded are checked.
///
/// `published` carries the downloaded PUBLISHED bytes and the uploaded-name
/// mapping (see [`PublishedSignatureSource`]); an asset without downloaded
/// bytes is verified from its locally-produced file.
///
/// Infallible by design: any condition that prevents checking an asset
/// (including config-render errors, which the expected-asset derivation
/// already reports as issues elsewhere) logs a verbose notice and yields no
/// outcome for it. See the module docs for why.
pub fn verify_signature_assets(
    ctx: &Context,
    crate_name: &str,
    release_ids: Option<&[String]>,
    published: &PublishedSignatureSource,
    log: &StageLogger,
) -> SignatureVerification {
    let mut result = SignatureVerification::default();
    if ctx.should_skip("sign") || ctx.is_dry_run() {
        return result;
    }
    let skips = ctx.skip_memento.snapshot();

    for (sign_idx, cfg) in ctx.config.signs.iter().enumerate() {
        let sub_label = cfg
            .id
            .clone()
            .unwrap_or_else(|| format!("sign[{sign_idx}]"));
        let skip_config = |reason: &str| {
            log.verbose(&format!(
                "signature re-verification skipped for sign config '{sub_label}' — {reason}"
            ));
        };
        if skips
            .iter()
            .any(|e| e.stage == "sign" && e.label == sub_label)
        {
            continue;
        }
        // Authenticode signs IN PLACE (no detached signature asset exists on
        // the release to re-verify).
        if cfg.authenticode.is_some() {
            continue;
        }
        let proceed = anodizer_core::config::evaluate_if_condition(
            cfg.if_condition.as_deref(),
            &format!("sign '{sub_label}' (signature re-verification)"),
            |t| ctx.render_template(t),
        );
        match proceed {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                skip_config(&format!("could not evaluate `if:`: {e:#}"));
                continue;
            }
        }
        let filter = cfg.resolved_artifacts(SignConfig::DEFAULT_ARTIFACTS);
        if filter == "none" {
            continue;
        }

        let cmd = cfg.cmd.clone().unwrap_or_else(default_sign_cmd);
        let args = cfg.resolved_args();
        let mode = resolve_config_verify_mode(
            cfg.verify.as_ref(),
            &cmd,
            &args,
            cfg.certificate.is_some(),
            ctx.env_source(),
        );
        match &mode {
            ConfigVerifyMode::Disabled => {
                skip_config("disabled by `verify.enabled: false`");
                continue;
            }
            ConfigVerifyMode::Skip(reason) => {
                skip_config(reason);
                continue;
            }
            _ => {}
        }
        if !anodizer_core::tool_detect::on_path(&cmd) {
            skip_config(&format!("verifier '{cmd}' is not available"));
            continue;
        }
        let mut env = match cfg
            .env
            .as_deref()
            .map(|entries| {
                anodizer_core::config::render_env_entries(entries, |v| ctx.render_template(v))
            })
            .transpose()
        {
            Ok(pairs) => pairs.unwrap_or_default(),
            Err(e) => {
                skip_config(&format!("could not render `env:`: {e:#}"));
                continue;
            }
        };
        ensure_cosign_consent_env(&cmd, &mut env);

        // Pair each eligible payload artifact with the signature /
        // certificate paths this config derives for it — the same
        // per-artifact rendering the sign stage used to write them.
        let mut pairs: Vec<(
            std::path::PathBuf,
            std::path::PathBuf,
            Option<std::path::PathBuf>,
        )> = Vec::new();
        let mut pairing_ok = true;
        for artifact in ctx.artifacts.all().iter() {
            if artifact.crate_name != crate_name {
                continue;
            }
            if anodizer_core::artifact::is_directory_bundle_artifact(artifact) {
                continue;
            }
            match should_sign_artifact(artifact.kind, filter) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    skip_config(&format!("could not resolve artifact filter: {e:#}"));
                    pairing_ok = false;
                    break;
                }
            }
            if !sign_ids_match(&artifact.metadata, cfg.ids.as_ref()) {
                continue;
            }
            if !anodizer_core::artifact::matches_id_filter(artifact, release_ids) {
                continue;
            }
            match expected_output_paths(cfg, &artifact.path, &artifact.metadata, ctx) {
                Ok((sig, cert)) => pairs.push((artifact.path.clone(), sig, cert)),
                Err(e) => {
                    skip_config(&format!("could not derive signature paths: {e:#}"));
                    pairing_ok = false;
                    break;
                }
            }
        }
        if !pairing_ok || pairs.is_empty() {
            continue;
        }

        // Keyed cosign verification consumes the PUBLIC half of the signing
        // key, derived once per config. Material that fails to load here
        // (e.g. the key env var is absent in this leg) is an environmental
        // shortfall — fall back, never fail.
        let pubkey_file: Option<tempfile::NamedTempFile> = match &mode {
            ConfigVerifyMode::CosignKeyed { key_ref, .. } => {
                let tmp = match tempfile::Builder::new()
                    .prefix("anodizer-reverify-")
                    .suffix(".pub")
                    .tempfile()
                {
                    Ok(t) => t,
                    Err(e) => {
                        skip_config(&format!("could not create public-key temp file: {e}"));
                        continue;
                    }
                };
                match derive_cosign_public_key(&cmd, key_ref, Some(&env), tmp.path()) {
                    Ok(()) => Some(tmp),
                    Err(e) => {
                        skip_config(&format!("verification material unavailable: {e:#}"));
                        continue;
                    }
                }
            }
            _ => None,
        };
        let pubkey_path: Option<String> = pubkey_file
            .as_ref()
            .map(|f| f.path().to_string_lossy().into_owned());

        // The certificate participates in verification only for non-bundle
        // keyless cosign, where it is passed via `--certificate`.
        let cert_consumed = matches!(
            &mode,
            ConfigVerifyMode::CosignKeyless {
                bundle: false,
                has_certificate: true,
                ..
            }
        );

        for (payload, sig_path, cert_path) in &pairs {
            let sig_name = published.published_name(sig_path);
            let skip_asset = |reason: &str| {
                log.verbose(&format!(
                    "signature re-verification skipped for '{sig_name}' — {reason}"
                ));
            };
            if !payload.is_file() {
                skip_asset("its payload artifact is not on disk");
                continue;
            }
            // The bytes consumers download are the ones that matter, so the
            // downloaded published copy wins when the caller supplied one;
            // otherwise the locally-produced file still proves material and
            // payload agree.
            let sig_file = published
                .downloaded
                .get(&sig_name)
                .unwrap_or(sig_path)
                .clone();
            let sig_nonempty = std::fs::metadata(&sig_file).is_ok_and(|m| m.len() > 0);
            if !sig_nonempty {
                skip_asset("the signature file is missing or empty on disk");
                continue;
            }
            let cert_name = cert_path.as_ref().map(|c| published.published_name(c));
            let cert_file: Option<std::path::PathBuf> = cert_path.as_ref().map(|c| {
                cert_name
                    .as_ref()
                    .and_then(|n| published.downloaded.get(n))
                    .unwrap_or(c)
                    .clone()
            });
            let cert_str = cert_file.as_ref().map(|c| c.to_string_lossy().into_owned());
            if cert_consumed && !cert_file.as_ref().is_some_and(|c| c.is_file()) {
                skip_asset("its keyless certificate file is not on disk");
                continue;
            }
            let Some(vargs) = build_blob_verify_args(
                &mode,
                &payload.to_string_lossy(),
                &sig_file.to_string_lossy(),
                cert_str.as_deref(),
                pubkey_path.as_deref(),
            ) else {
                continue;
            };
            let job = VerifyJob {
                cmd: cmd.clone(),
                args: vargs,
                env: Some(env.clone()),
                what: format!("published signature asset '{sig_name}'"),
            };
            match execute_verify_job_classified(&job, log) {
                VerifyRunVerdict::Verified => {
                    result.record(sig_name.clone(), SignatureCryptoOutcome::Verified);
                    if cert_consumed && let Some(name) = cert_name {
                        result.record(name, SignatureCryptoOutcome::Verified);
                    }
                }
                VerifyRunVerdict::Invalid(reason) => {
                    result.record(sig_name.clone(), SignatureCryptoOutcome::Invalid(reason));
                }
                VerifyRunVerdict::Inconclusive(reason) => {
                    skip_asset(&format!("verifier could not judge it: {reason}"));
                }
            }
        }
    }
    result
}

/// Asset basename of an output path.
fn basename(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::SignVerifyConfig;
    use anodizer_core::test_helpers::TestContextBuilder;
    use std::collections::HashMap;

    /// Write an executable shell script and return its path.
    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
        path
    }

    /// Recording verifier stub: appends each invocation's argv to
    /// `$STATE/calls` and exits 0.
    fn recording_stub(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        write_script(
            dir,
            name,
            "#!/bin/sh\necho \"$@\" >> \"$STATE/calls\"\nexit 0\n",
        )
    }

    fn calls(state: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(state.join("calls"))
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    fn add_file_artifact(
        ctx: &mut Context,
        dir: &std::path::Path,
        kind: ArtifactKind,
        name: &str,
        crate_name: &str,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, name.as_bytes()).expect("artifact bytes");
        ctx.artifacts.add(Artifact {
            kind,
            name: name.to_string(),
            path: path.clone(),
            target: None,
            crate_name: crate_name.to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        path
    }

    fn ctx_with(dist: &std::path::Path, signs: Vec<SignConfig>) -> Context {
        TestContextBuilder::new()
            .tag("v1.0.0")
            .dry_run(false)
            .signs(signs)
            .dist(dist.to_path_buf())
            .sealed_env()
            .build()
    }

    fn gpg_config(cmd: &std::path::Path, state: &std::path::Path) -> SignConfig {
        SignConfig {
            cmd: Some(cmd.to_string_lossy().to_string()),
            artifacts: Some("archive".to_string()),
            args: Some(vec![
                "--output".to_string(),
                "{{ .Signature }}".to_string(),
                "--detach-sig".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            env: Some(vec![format!("STATE={}", state.display())]),
            ..Default::default()
        }
    }

    #[test]
    fn gpg_signature_pairs_with_its_payload() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = recording_stub(tmp.path(), "gpg");

        let mut ctx = ctx_with(tmp.path(), vec![gpg_config(&stub, &state)]);
        let payload = add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Archive,
            "app.tar.gz",
            "app",
        );
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Signature,
            "app.tar.gz.sig",
            "app",
        );

        let log = ctx.logger("verify-release");
        let result = verify_signature_assets(
            &ctx,
            "app",
            None,
            &PublishedSignatureSource::default(),
            &log,
        );

        assert_eq!(
            result.outcome("app.tar.gz.sig"),
            Some(&SignatureCryptoOutcome::Verified),
            "the signature asset must carry a Verified outcome"
        );
        let calls = calls(&state);
        assert_eq!(
            calls,
            vec![format!(
                "--verify {}.sig {}",
                payload.display(),
                payload.display()
            )],
            "gpg must be invoked with the signature paired to ITS payload"
        );
    }

    #[test]
    fn keyless_certificate_pairs_and_is_credited() {
        // Non-bundle keyless cosign consumes the certificate on the verify
        // argv; both the .sig and the .pem asset must be credited as
        // verified from the one verifier run.
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = recording_stub(tmp.path(), "cosign");

        let cfg = SignConfig {
            cmd: Some(stub.to_string_lossy().to_string()),
            artifacts: Some("archive".to_string()),
            args: Some(vec![
                "sign-blob".to_string(),
                "--output-signature".to_string(),
                "{{ .Signature }}".to_string(),
                "--output-certificate".to_string(),
                "{{ .Certificate }}".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            certificate: Some("{{ .Artifact }}.pem".to_string()),
            env: Some(vec![format!("STATE={}", state.display())]),
            verify: Some(SignVerifyConfig {
                certificate_identity: Some("https://example.com/wf".to_string()),
                certificate_oidc_issuer: Some("https://issuer.example".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut ctx = ctx_with(tmp.path(), vec![cfg]);
        let payload = add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Archive,
            "app.tar.gz",
            "app",
        );
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Signature,
            "app.tar.gz.sig",
            "app",
        );
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Certificate,
            "app.tar.gz.pem",
            "app",
        );

        let log = ctx.logger("verify-release");
        let result = verify_signature_assets(
            &ctx,
            "app",
            None,
            &PublishedSignatureSource::default(),
            &log,
        );

        assert_eq!(
            result.outcome("app.tar.gz.sig"),
            Some(&SignatureCryptoOutcome::Verified)
        );
        assert_eq!(
            result.outcome("app.tar.gz.pem"),
            Some(&SignatureCryptoOutcome::Verified),
            "the consumed certificate must be credited by the same verifier run"
        );
        let calls = calls(&state);
        assert_eq!(
            calls,
            vec![format!(
                "verify-blob --certificate {p}.pem --signature {p}.sig \
                 --certificate-identity https://example.com/wf \
                 --certificate-oidc-issuer https://issuer.example {p}",
                p = payload.display()
            )],
            "keyless verify argv must pair certificate, signature, and payload"
        );
    }

    #[test]
    fn dynamic_tail_signature_template_still_pairs() {
        // A template whose final segment ends in an expansion has no static
        // suffix to glob on, but per-artifact rendering still resolves the
        // exact output name — pairing must not depend on a static suffix.
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = recording_stub(tmp.path(), "gpg");

        let cfg = SignConfig {
            signature: Some("{{ .Artifact }}.sig-{{ Version }}".to_string()),
            ..gpg_config(&stub, &state)
        };
        assert_eq!(
            anodizer_core::signature_assets::signature_template_suffix(
                "{{ .Artifact }}.sig-{{ Version }}"
            ),
            None,
            "sanity: this template must be one the static-suffix derivation cannot anchor"
        );
        let mut ctx = ctx_with(tmp.path(), vec![cfg]);
        let payload = add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Archive,
            "app.tar.gz",
            "app",
        );
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Signature,
            "app.tar.gz.sig-1.0.0",
            "app",
        );

        let log = ctx.logger("verify-release");
        let result = verify_signature_assets(
            &ctx,
            "app",
            None,
            &PublishedSignatureSource::default(),
            &log,
        );

        assert_eq!(
            result.outcome("app.tar.gz.sig-1.0.0"),
            Some(&SignatureCryptoOutcome::Verified),
            "the rendered dynamic-tail signature name must pair and verify"
        );
        assert_eq!(
            calls(&state),
            vec![format!("--verify {p}.sig-1.0.0 {p}", p = payload.display())]
        );
    }

    #[test]
    fn verification_is_scoped_to_the_requested_crate() {
        // Workspace modes (lockstep and per-crate) verify each crate's own
        // artifacts only; another crate's signatures are out of scope for
        // this crate's pass and get their own pass.
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = recording_stub(tmp.path(), "gpg");

        let mut ctx = ctx_with(tmp.path(), vec![gpg_config(&stub, &state)]);
        let a_payload = add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Archive,
            "a.tar.gz",
            "crate-a",
        );
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Signature,
            "a.tar.gz.sig",
            "crate-a",
        );
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Archive,
            "b.tar.gz",
            "crate-b",
        );
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Signature,
            "b.tar.gz.sig",
            "crate-b",
        );

        let log = ctx.logger("verify-release");
        let result = verify_signature_assets(
            &ctx,
            "crate-a",
            None,
            &PublishedSignatureSource::default(),
            &log,
        );

        assert_eq!(
            result.outcome("a.tar.gz.sig"),
            Some(&SignatureCryptoOutcome::Verified)
        );
        assert_eq!(
            result.outcome("b.tar.gz.sig"),
            None,
            "crate-b's signature is out of crate-a's scope"
        );
        assert_eq!(
            calls(&state),
            vec![format!("--verify {p}.sig {p}", p = a_payload.display())],
            "only crate-a's payload may be verified in crate-a's pass"
        );
    }

    #[test]
    fn keyed_material_that_fails_to_load_yields_no_outcome() {
        // `cosign public-key` failing (key env var absent in this leg) is an
        // environmental shortfall: no verifier may run and no asset may be
        // marked invalid.
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        // Stub fails `public-key` and records any OTHER invocation, proving
        // no verify-blob is attempted after the failed derivation.
        let stub = write_script(
            tmp.path(),
            "cosign",
            concat!(
                "#!/bin/sh\n",
                "case \"$1\" in public-key) echo 'reading key: no key' >&2; exit 1 ;; esac\n",
                "echo \"$@\" >> \"$STATE/calls\"\n",
                "exit 0\n",
            ),
        );
        let cfg = SignConfig {
            cmd: Some(stub.to_string_lossy().to_string()),
            artifacts: Some("archive".to_string()),
            args: Some(vec![
                "sign-blob".to_string(),
                "--key".to_string(),
                "env://ANODIZER_TEST_ABSENT_KEY".to_string(),
                "--output-signature".to_string(),
                "{{ .Signature }}".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            env: Some(vec![format!("STATE={}", state.display())]),
            ..Default::default()
        };
        let mut ctx = ctx_with(tmp.path(), vec![cfg]);
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Archive,
            "app.tar.gz",
            "app",
        );
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Signature,
            "app.tar.gz.sig",
            "app",
        );

        let log = ctx.logger("verify-release");
        let result = verify_signature_assets(
            &ctx,
            "app",
            None,
            &PublishedSignatureSource::default(),
            &log,
        );

        assert_eq!(
            result.outcome("app.tar.gz.sig"),
            None,
            "unloadable key material must yield no outcome (fallback), never Invalid"
        );
        assert!(
            calls(&state).is_empty(),
            "no verify invocation may run after the public-key derivation failed"
        );
    }

    #[test]
    fn positive_verification_wins_when_configs_collide_on_one_output() {
        // Two sign configs deriving the SAME signature path: the on-disk
        // bytes are the last writer's, so the first config's verifier
        // legitimately rejects them. One positive verify under trusted
        // material must win over that collision rejection.
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let bad = write_script(
            tmp.path(),
            "gpg-bad",
            concat!(
                "#!/bin/sh\n",
                "echo \"$@\" >> \"$STATE/calls\"\n",
                "echo 'gpg: BAD signature from \"Second Signer\"' >&2\n",
                "exit 1\n",
            ),
        );
        let good = recording_stub(tmp.path(), "gpg-good");

        let mut ctx = ctx_with(
            tmp.path(),
            vec![gpg_config(&bad, &state), gpg_config(&good, &state)],
        );
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Archive,
            "app.tar.gz",
            "app",
        );
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Signature,
            "app.tar.gz.sig",
            "app",
        );

        let log = ctx.logger("verify-release");
        let result = verify_signature_assets(
            &ctx,
            "app",
            None,
            &PublishedSignatureSource::default(),
            &log,
        );

        assert_eq!(
            result.outcome("app.tar.gz.sig"),
            Some(&SignatureCryptoOutcome::Verified),
            "a positive verification by any producing config must win over \
             another config's collision rejection"
        );
        assert_eq!(
            calls(&state).len(),
            2,
            "both configs must have run their verifier"
        );
    }

    /// Keyed-cosign stub: `public-key` writes a fake key to `--outfile` and
    /// succeeds; every other invocation is recorded and fails with `body`
    /// on stderr at `exit_code`.
    fn keyed_cosign_stub(
        dir: &std::path::Path,
        stderr_line: &str,
        exit_code: i32,
    ) -> std::path::PathBuf {
        write_script(
            dir,
            "cosign",
            &format!(
                concat!(
                    "#!/bin/sh\n",
                    "if [ \"$1\" = public-key ]; then\n",
                    "  out=\n",
                    "  while [ $# -gt 0 ]; do\n",
                    "    [ \"$1\" = --outfile ] && out=\"$2\"\n",
                    "    shift\n",
                    "  done\n",
                    "  echo fake-public-key > \"$out\"\n",
                    "  exit 0\n",
                    "fi\n",
                    "echo \"$@\" >> \"$STATE/calls\"\n",
                    "echo '{stderr_line}' >&2\n",
                    "exit {exit_code}\n",
                ),
                stderr_line = stderr_line,
                exit_code = exit_code,
            ),
        )
    }

    fn keyed_cosign_config(cmd: &std::path::Path, state: &std::path::Path) -> SignConfig {
        SignConfig {
            cmd: Some(cmd.to_string_lossy().to_string()),
            artifacts: Some("archive".to_string()),
            args: Some(vec![
                "sign-blob".to_string(),
                "--key".to_string(),
                "env://COSIGN_KEY".to_string(),
                "--output-signature".to_string(),
                "{{ .Signature }}".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            env: Some(vec![format!("STATE={}", state.display())]),
            ..Default::default()
        }
    }

    #[test]
    fn keyed_cosign_transient_tlog_failure_yields_no_outcome() {
        // The verifier RAN and failed with a network wording — cosign also
        // uses `failed to verify signature` for SCT/tlog fetch errors, which
        // proves nothing about the bytes. Fallback, never Invalid.
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = keyed_cosign_stub(
            tmp.path(),
            "failed to verify signature: Get \"https://rekor.sigstore.dev/api/v1/log/entries\": dial tcp: connection refused",
            1,
        );

        let mut ctx = ctx_with(tmp.path(), vec![keyed_cosign_config(&stub, &state)]);
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Archive,
            "app.tar.gz",
            "app",
        );
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Signature,
            "app.tar.gz.sig",
            "app",
        );

        let log = ctx.logger("verify-release");
        let result = verify_signature_assets(
            &ctx,
            "app",
            None,
            &PublishedSignatureSource::default(),
            &log,
        );

        assert_eq!(
            result.outcome("app.tar.gz.sig"),
            None,
            "a tlog/network verifier failure must yield no outcome (fallback)"
        );
        let calls = calls(&state);
        assert_eq!(calls.len(), 1, "the verifier must actually have run");
        assert!(
            calls[0].starts_with("verify-blob --key "),
            "keyed verify argv expected, got: {}",
            calls[0]
        );
    }

    #[test]
    fn gpg_environmental_exit_two_yields_no_outcome() {
        // gpg reserves exit 1 for a bad signature; exit 2 is environmental
        // (missing public key here) and must fall back, never fail.
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = write_script(
            tmp.path(),
            "gpg",
            concat!(
                "#!/bin/sh\n",
                "echo \"$@\" >> \"$STATE/calls\"\n",
                "echo \"gpg: Can't check signature: No public key\" >&2\n",
                "exit 2\n",
            ),
        );

        let mut ctx = ctx_with(tmp.path(), vec![gpg_config(&stub, &state)]);
        let payload = add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Archive,
            "app.tar.gz",
            "app",
        );
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Signature,
            "app.tar.gz.sig",
            "app",
        );

        let log = ctx.logger("verify-release");
        let result = verify_signature_assets(
            &ctx,
            "app",
            None,
            &PublishedSignatureSource::default(),
            &log,
        );

        assert_eq!(
            result.outcome("app.tar.gz.sig"),
            None,
            "gpg exit 2 (environmental) must yield no outcome (fallback)"
        );
        assert_eq!(
            calls(&state),
            vec![format!("--verify {p}.sig {p}", p = payload.display())],
            "the verifier must actually have run"
        );
    }

    #[test]
    fn keyed_bundle_verify_argv_consumes_bundle_flag() {
        // A `--bundle` sign argv must verify via `--bundle <sig>` (the
        // signature asset IS the sigstore bundle), not `--signature`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = keyed_cosign_stub(tmp.path(), "unused", 0);

        let cfg = SignConfig {
            args: Some(vec![
                "sign-blob".to_string(),
                "--key".to_string(),
                "env://COSIGN_KEY".to_string(),
                "--bundle".to_string(),
                "{{ .Signature }}".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            ..keyed_cosign_config(&stub, &state)
        };
        let mut ctx = ctx_with(tmp.path(), vec![cfg]);
        let payload = add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Archive,
            "app.tar.gz",
            "app",
        );
        let sig = add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Signature,
            "app.tar.gz.sig",
            "app",
        );

        let log = ctx.logger("verify-release");
        let result = verify_signature_assets(
            &ctx,
            "app",
            None,
            &PublishedSignatureSource::default(),
            &log,
        );

        assert_eq!(
            result.outcome("app.tar.gz.sig"),
            Some(&SignatureCryptoOutcome::Verified)
        );
        let calls = calls(&state);
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].starts_with("verify-blob --key "),
            "keyed argv expected, got: {}",
            calls[0]
        );
        assert!(
            calls[0].ends_with(&format!("--bundle {} {}", sig.display(), payload.display())),
            "bundle argv expected, got: {}",
            calls[0]
        );
    }

    fn keyless_bundle_config(cmd: &std::path::Path, state: &std::path::Path) -> SignConfig {
        SignConfig {
            cmd: Some(cmd.to_string_lossy().to_string()),
            artifacts: Some("archive".to_string()),
            args: Some(vec![
                "sign-blob".to_string(),
                "--bundle".to_string(),
                "{{ .Signature }}".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            env: Some(vec![format!("STATE={}", state.display())]),
            verify: Some(SignVerifyConfig {
                certificate_identity: Some("https://example.com/wf".to_string()),
                certificate_oidc_issuer: Some("https://issuer.example".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn keyless_bundle_verify_argv_pairs_bundle_and_identity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = recording_stub(tmp.path(), "cosign");

        let mut ctx = ctx_with(tmp.path(), vec![keyless_bundle_config(&stub, &state)]);
        let payload = add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Archive,
            "app.tar.gz",
            "app",
        );
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Signature,
            "app.tar.gz.sig",
            "app",
        );

        let log = ctx.logger("verify-release");
        let result = verify_signature_assets(
            &ctx,
            "app",
            None,
            &PublishedSignatureSource::default(),
            &log,
        );

        assert_eq!(
            result.outcome("app.tar.gz.sig"),
            Some(&SignatureCryptoOutcome::Verified)
        );
        assert_eq!(
            calls(&state),
            vec![format!(
                "verify-blob --bundle {p}.sig \
                 --certificate-identity https://example.com/wf \
                 --certificate-oidc-issuer https://issuer.example {p}",
                p = payload.display()
            )],
            "keyless bundle argv must pair bundle, identity, and payload"
        );
    }

    #[test]
    fn bundle_verify_unmarked_failure_yields_no_outcome() {
        // A bundle-verify failure whose wording matches no crypto-layer
        // marker cannot prove the bytes bad — fallback, never Invalid.
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = write_script(
            tmp.path(),
            "cosign",
            concat!(
                "#!/bin/sh\n",
                "echo \"$@\" >> \"$STATE/calls\"\n",
                "echo 'Error: verifying blob: unmarshalling bundle: unexpected media type' >&2\n",
                "exit 1\n",
            ),
        );

        let mut ctx = ctx_with(tmp.path(), vec![keyless_bundle_config(&stub, &state)]);
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Archive,
            "app.tar.gz",
            "app",
        );
        add_file_artifact(
            &mut ctx,
            tmp.path(),
            ArtifactKind::Signature,
            "app.tar.gz.sig",
            "app",
        );

        let log = ctx.logger("verify-release");
        let result = verify_signature_assets(
            &ctx,
            "app",
            None,
            &PublishedSignatureSource::default(),
            &log,
        );

        assert_eq!(
            result.outcome("app.tar.gz.sig"),
            None,
            "an unmarked bundle-verify failure must yield no outcome (fallback)"
        );
        assert_eq!(calls(&state).len(), 1, "the verifier must have run");
    }
}
