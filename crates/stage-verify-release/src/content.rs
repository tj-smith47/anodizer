use super::*;

/// Cap on the bytes the digest-fallback path will download and hash when
/// GitHub exposes no `sha256:` digest for an asset. Beyond this the check
/// stays honest but cheaper: size-only, with a verbose notice. 64 MiB covers
/// typical release binaries/archives without turning the gate into a full
/// re-download of multi-hundred-MB artifacts.
const DIGEST_DOWNLOAD_CAP: u64 = 64 * 1024 * 1024;

/// Compare each expected asset that IS present on the release against its
/// local bytes: stored size must equal the local file size, and the stored
/// `sha256:` digest (when GitHub serves one) must equal the local sha256.
/// When no digest is served, small assets are downloaded and hashed instead;
/// larger ones are verified by size only, with a verbose notice.
///
/// `assets_published_by_this_run` is false only for the pre-submitter gate in
/// a leg whose assets an earlier leg uploaded; it exempts the
/// publish-surface-dependent combined checksum manifests (and their
/// signatures' cryptographic re-check, whose payload is that manifest) from
/// cross-leg byte comparison — see [`surface_dependent_asset_names`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn verify_published_contents(
    ctx: &Context,
    log: &StageLogger,
    crate_cfg: &CrateConfig,
    release_cfg: &anodizer_core::config::ReleaseConfig,
    expected: &[String],
    published: &[anodizer_stage_release::PublishedAsset],
    assets_published_by_this_run: bool,
    issues: &mut Vec<String>,
) -> ContentSummary {
    // Empty when this run uploaded the assets, so the `contains` checks below
    // collapse to no-ops on the normal (same-leg) path.
    let surface_dependent = if assets_published_by_this_run {
        std::collections::BTreeSet::new()
    } else {
        surface_dependent_asset_names(
            ctx,
            &crate_cfg.name,
            release_cfg.ids.as_deref(),
            release_cfg.exclude.as_deref(),
        )
    };
    let local = local_asset_index(
        ctx,
        &crate_cfg.name,
        release_cfg.ids.as_deref(),
        release_cfg.exclude.as_deref(),
    );
    let signature_suffixes = anodizer_core::signature_assets::signature_asset_suffixes(&ctx.config);
    let mut summary = ContentSummary {
        issue_count: 0,
        digest_unverified: 0,
    };
    // A locally-registered asset is classified by its exact `ArtifactKind`
    // (`Signature` / `Certificate`); an asset with no local entry (uploaded
    // from a prior run, or produced outside this invocation) falls back to
    // the suffix set, since no kind signal exists for it. The suffix set's
    // dynamic-tail templates can false-fail this fallback, but a false
    // digest-mismatch report is the safer failure mode than silently
    // exempting real content.
    let classify_signature = |name: &str| -> bool {
        match local.get(name) {
            Some((_, _, kind)) => {
                matches!(kind, ArtifactKind::Signature | ArtifactKind::Certificate)
            }
            None => anodizer_core::signature_assets::is_signature_asset(name, &signature_suffixes),
        }
    };
    // Cryptographic re-verification of signature assets, resolved lazily so
    // a release without a single present signature asset never spawns a
    // verifier, derives a public key, or downloads anything.
    let mut crypto: Option<anodizer_stage_sign::SignatureVerification> = None;
    for name in expected {
        let Some(asset) = published.iter().find(|p| &p.name == name) else {
            continue;
        };
        let is_signature = classify_signature(name);
        if is_signature {
            // GPG/cosign signatures embed a timestamp or random nonce, so a
            // resign of byte-identical input never reproduces the same
            // bytes — a digest comparison would flag every re-published
            // signature as "mismatched" even though nothing is wrong.
            // Presence is still enforced upstream in the missing-asset
            // diff; this only exempts the byte-level comparison.
            // This is a DELIBERATE, narrower exemption than it looks: it
            // applies only inside this `if is_signature` arm. Every other
            // (payload) asset falls through to the digest/size comparison
            // below — `is_signature_asset`/the exact `ArtifactKind` match
            // above are the only gate, so a payload asset can never take
            // this shortcut and skip its digest check.
            if asset.size == 0 {
                issues.push(format!(
                    "signature/certificate asset '{name}' of crate '{}' is empty (0 bytes) — \
                     the signing step likely failed silently",
                    crate_cfg.name
                ));
                summary.issue_count += 1;
                continue;
            }
            // A signature over a surface-dependent payload (the combined
            // checksum manifest) cannot be cryptographically re-checked
            // cross-leg: the published signature signs the UPLOADING leg's
            // manifest bytes, while this leg holds its own recomputed
            // manifest — verifying one against the other would reject a
            // perfectly valid signature. Presence + non-empty were enforced
            // above; the byte-level truth was established by the leg that
            // signed and uploaded it.
            if surface_dependent
                .iter()
                .any(|s| name.starts_with(s.as_str()) && name.len() > s.len())
            {
                log.verbose(&format!(
                    "asset '{name}' signs a publish-surface-dependent manifest \
                     uploaded by an earlier leg — cryptographic re-check \
                     exempted (present and non-empty)"
                ));
                continue;
            }
            // In place of the exempted digest comparison, the signature is
            // re-verified CRYPTOGRAPHICALLY against its payload with
            // material derived from the resolved `signs:` config (keyed
            // cosign public key, keyless identity, gpg keyring). The
            // PUBLISHED signature bytes are downloaded and verified against
            // the local payload (whose equality with the published payload
            // the digest check establishes), so a signature corrupted or
            // replaced on the release is caught; a failed download degrades
            // to verifying the locally-produced bytes. Only a POSITIVE
            // rejection fails; any environmental shortfall (tool or key
            // material absent in this leg, download unavailable) falls back
            // to the presence + non-empty check above.
            let verification = crypto.get_or_insert_with(|| {
                let download_dir = tempfile::Builder::new()
                    .prefix("anodizer-verify-sig-")
                    .tempdir();
                let source = match &download_dir {
                    Ok(dir) => published_signature_source(
                        &local,
                        published,
                        expected,
                        &classify_signature,
                        ctx.options.token.as_deref(),
                        dir.path(),
                        log,
                    ),
                    Err(e) => {
                        log.verbose(&format!(
                            "could not create a download dir for published signature \
                             bytes ({e}) — verifying locally-produced bytes instead"
                        ));
                        anodizer_stage_sign::PublishedSignatureSource::default()
                    }
                };
                anodizer_stage_sign::verify_signature_assets(
                    ctx,
                    &crate_cfg.name,
                    release_cfg.ids.as_deref(),
                    &source,
                    log,
                )
            });
            match verification.outcome(name) {
                Some(anodizer_stage_sign::SignatureCryptoOutcome::Verified) => {
                    log.status(&format!(
                        "verified signature '{name}' (cryptographic check)"
                    ));
                }
                Some(anodizer_stage_sign::SignatureCryptoOutcome::Invalid(reason)) => {
                    issues.push(format!(
                        "signature/certificate asset '{name}' of crate '{}' FAILED \
                         cryptographic verification: {reason} — the signature does not \
                         verify against the artifact it signs",
                        crate_cfg.name
                    ));
                    summary.issue_count += 1;
                }
                None => {
                    log.verbose(&format!(
                        "asset '{name}' is a signature/certificate — present and non-empty, \
                         digest comparison exempted (no cryptographic verdict was derivable: \
                         the verifier tool, key material, or producing sign config is \
                         unavailable in this environment)"
                    ));
                }
            }
            continue;
        }
        // Cross-leg exemption for the combined checksum manifest itself: the
        // uploading leg rewrote it with publish-time evidence (docker
        // digests) this leg's surface never produces, so this leg's locally
        // recomputed bytes legitimately differ from the published ones.
        // Presence was already enforced by the missing-asset diff.
        if surface_dependent.contains(name) {
            log.verbose(&format!(
                "asset '{name}' is a publish-surface-dependent manifest \
                 uploaded by an earlier leg — byte comparison exempted \
                 (its bytes fold in that leg's publish-time evidence)"
            ));
            continue;
        }
        let Some((path, meta_sha, _kind)) = local.get(name) else {
            log.verbose(&format!(
                "no local file registered for asset '{name}' — name-only check"
            ));
            continue;
        };
        let local_size = match std::fs::metadata(path) {
            Ok(md) => md.len(),
            Err(e) => {
                log.verbose(&format!(
                    "local file {} for asset '{name}' unreadable ({e}) — name-only check",
                    path.display()
                ));
                continue;
            }
        };
        // A checksum-stage sha256 is reused when present; otherwise the local
        // file is hashed here (cheap relative to the release it verifies).
        let local_sha = match meta_sha {
            Some(s) => s.clone(),
            None => match anodizer_core::hashing::sha256_file(path) {
                Ok(s) => s,
                Err(e) => {
                    issues.push(format!(
                        "could not hash local file {} for asset '{name}' of crate '{}': {e:#}",
                        path.display(),
                        crate_cfg.name
                    ));
                    summary.issue_count += 1;
                    continue;
                }
            },
        };
        match check_asset_content(local_size, &local_sha, asset.size, asset.digest.as_deref()) {
            ContentVerdict::Match => {
                log.verbose(&format!("asset '{name}' size+digest match"));
            }
            ContentVerdict::SizeMismatch { local, published } => {
                issues.push(format!(
                    "asset '{name}' of crate '{}' size mismatch: local {local} B vs \
                     published {published} B — the uploaded asset does not match the \
                     produced artifact",
                    crate_cfg.name
                ));
                summary.issue_count += 1;
            }
            ContentVerdict::DigestMismatch { local, published } => {
                issues.push(format!(
                    "asset '{name}' of crate '{}' digest mismatch: local sha256 {local} \
                     vs published sha256 {published} — the uploaded asset does not \
                     match the produced artifact",
                    crate_cfg.name
                ));
                summary.issue_count += 1;
            }
            ContentVerdict::DigestUnavailable => {
                if asset.size > DIGEST_DOWNLOAD_CAP {
                    log.verbose(&format!(
                        "asset '{name}' digest field unavailable and asset too large \
                         to download — verified size only"
                    ));
                    summary.digest_unverified += 1;
                    continue;
                }
                match download_sha256(
                    &asset.download_url,
                    ctx.options.token.as_deref(),
                    DIGEST_DOWNLOAD_CAP,
                ) {
                    Ok(remote_sha) if remote_sha.eq_ignore_ascii_case(&local_sha) => {
                        log.verbose(&format!(
                            "asset '{name}' digest verified via download (no digest field)"
                        ));
                    }
                    Ok(remote_sha) => {
                        issues.push(format!(
                            "asset '{name}' of crate '{}' digest mismatch (verified via \
                             download): local sha256 {local_sha} vs downloaded sha256 \
                             {remote_sha}",
                            crate_cfg.name
                        ));
                        summary.issue_count += 1;
                    }
                    Err(e) => {
                        issues.push(format!(
                            "could not download asset '{name}' of crate '{}' to verify \
                             its digest: {e:#}",
                            crate_cfg.name
                        ));
                        summary.issue_count += 1;
                    }
                }
            }
        }
    }
    summary
}

/// Size ceiling for downloading a published signature / certificate asset:
/// detached signatures, sigstore bundles, and PEM certificates are all far
/// below this; anything larger is not a signature worth pulling.
const SIGNATURE_DOWNLOAD_CAP: u64 = 4 * 1024 * 1024;

/// Build the published-bytes view the signature crypto check consumes: the
/// upload name each renamed local signature file maps to, plus a downloaded
/// copy of each present published signature asset. Any per-asset download
/// shortfall leaves the asset out of `downloaded` (its locally-produced
/// bytes are verified instead) with a verbose notice — never an issue.
fn published_signature_source(
    local: &std::collections::BTreeMap<String, (std::path::PathBuf, Option<String>, ArtifactKind)>,
    published: &[anodizer_stage_release::PublishedAsset],
    expected: &[String],
    is_signature: &dyn Fn(&str) -> bool,
    token: Option<&str>,
    download_dir: &std::path::Path,
    log: &StageLogger,
) -> anodizer_stage_sign::PublishedSignatureSource {
    let mut source = anodizer_stage_sign::PublishedSignatureSource::default();
    for (name, (path, _sha, kind)) in local {
        if matches!(kind, ArtifactKind::Signature | ArtifactKind::Certificate)
            && path.file_name().and_then(|n| n.to_str()) != Some(name.as_str())
        {
            source.uploaded_names.insert(path.clone(), name.clone());
        }
    }
    for (idx, name) in expected.iter().enumerate() {
        if !is_signature(name) {
            continue;
        }
        let Some(asset) = published.iter().find(|p| &p.name == name) else {
            continue;
        };
        if asset.size == 0 || asset.size > SIGNATURE_DOWNLOAD_CAP {
            continue;
        }
        // An index-derived filename, because the asset name is
        // remote-controlled data and must never influence the local path.
        let dest = download_dir.join(format!("published-{idx}"));
        match download_to_file(&asset.download_url, token, SIGNATURE_DOWNLOAD_CAP, &dest) {
            Ok(()) => {
                source.downloaded.insert(name.clone(), dest);
            }
            Err(e) => log.verbose(&format!(
                "could not download published signature asset '{name}' for \
                 cryptographic verification ({e:#}) — verifying the \
                 locally-produced bytes instead"
            )),
        }
    }
    source
}

/// Open an authenticated GET stream to a release asset, failing on any
/// non-success HTTP status.
fn asset_response(url: &str, token: Option<&str>) -> Result<reqwest::blocking::Response> {
    let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(120))?;
    let mut req = client.get(url).header("Accept", "application/octet-stream");
    if let Some(token) = token {
        // reqwest strips the Authorization header on the cross-host redirect
        // GitHub issues to its storage backend, so the token never leaks to
        // the presigned URL host.
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.send()?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("GET {url} returned HTTP {status}");
    }
    Ok(resp)
}

/// Download a release asset into `dest`, refusing to write more than `cap`
/// bytes.
fn download_to_file(
    url: &str,
    token: Option<&str>,
    cap: u64,
    dest: &std::path::Path,
) -> Result<()> {
    use std::io::Read as _;
    let mut reader = asset_response(url, token)?.take(cap + 1);
    let mut out = std::fs::File::create(dest)?;
    let copied = std::io::copy(&mut reader, &mut out)?;
    if copied > cap {
        anyhow::bail!("asset exceeds the {cap}-byte signature-download cap");
    }
    Ok(())
}

/// Download a release asset and return its sha256 hex, refusing to read more
/// than `cap` bytes — the digest fallback must never turn into an unbounded
/// re-download.
fn download_sha256(url: &str, token: Option<&str>, cap: u64) -> Result<String> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;
    let resp = asset_response(url, token)?;
    let mut hasher = Sha256::new();
    let mut reader = resp.take(cap + 1);
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > cap {
            anyhow::bail!("asset exceeds the {cap}-byte digest-download cap");
        }
        hasher.update(&buf[..n]);
    }
    Ok(anodizer_core::hashing::hex_lower(&hasher.finalize()))
}
