use std::io::Write as _;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;

use crate::provider::Provider;

// ---------------------------------------------------------------------------
// KMS provider detection and client-side encryption
// ---------------------------------------------------------------------------

/// Identifies how the KMS key should be used for encryption.
///
/// Supported KMS URL schemes: `awskms://`, `gcpkms://`, and `azurekeyvault://`
/// schemes via gocloud.dev/secrets for **client-side** encryption of blob data
/// before upload. A plain key ARN/ID (no scheme) means server-side encryption
/// (SSE-KMS on S3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KmsProvider {
    /// `awskms://key-id` or `awskms:///arn:aws:kms:...` — client-side via AWS CLI
    Aws,
    /// `gcpkms://projects/.../cryptKeys/...` — client-side via gcloud CLI
    Gcp,
    /// `azurekeyvault://vault-name/keys/key-name[/version]` — client-side via az CLI
    Azure,
    /// Plain key ARN/ID without URL scheme — server-side SSE-KMS (S3 only)
    ServerSide,
}

pub(crate) fn parse_kms_provider(kms_key: &str) -> KmsProvider {
    if kms_key.starts_with("awskms://") {
        KmsProvider::Aws
    } else if kms_key.starts_with("gcpkms://") {
        KmsProvider::Gcp
    } else if kms_key.starts_with("azurekeyvault://") {
        KmsProvider::Azure
    } else {
        KmsProvider::ServerSide
    }
}

/// Refuse a kms_key whose URL scheme cannot encrypt for `provider`.
///
/// `awskms://` only makes sense alongside `provider: s3` (AWS CLI signs with
/// AWS creds), `gcpkms://` alongside `gs`, `azurekeyvault://` alongside
/// `azblob`. A plain ARN/ID (ServerSide) is only accepted on S3 because GCS
/// and Azure object_store backends don't surface SSE-KMS through canned
/// headers. Catching this at config-time prevents a confusing
/// "AccessDenied"-or-similar failure deep inside the upload phase.
pub(crate) fn validate_kms_provider_match(
    provider: Provider,
    kms: KmsProvider,
    kms_key: &str,
) -> Result<()> {
    let ok = matches!(
        (provider, kms),
        (Provider::S3, KmsProvider::Aws | KmsProvider::ServerSide)
            | (Provider::Gcs, KmsProvider::Gcp)
            | (Provider::AzBlob, KmsProvider::Azure)
    );
    if ok {
        Ok(())
    } else {
        let want_scheme = match provider {
            Provider::S3 => "awskms:// (or a plain ARN for SSE-KMS)",
            Provider::Gcs => "gcpkms://",
            Provider::AzBlob => "azurekeyvault://",
        };
        anyhow::bail!(
            "blobs: kms_key '{}' is not compatible with provider '{}'; expected scheme: {}",
            kms_key,
            provider.display_name(),
            want_scheme
        );
    }
}

/// Verify the CLI tool needed for `provider`'s client-side encryption is on
/// PATH. Runs `<tool> --version` (or equivalent) once before fanning out so
/// missing-CLI failures surface at config-validate time, not deep inside the
/// upload phase where the failure shape is opaque.
pub(crate) fn preflight_kms_cli(provider: KmsProvider) -> Result<()> {
    let tool = match provider {
        KmsProvider::Aws => "aws",
        KmsProvider::Gcp => "gcloud",
        KmsProvider::Azure => "az",
        KmsProvider::ServerSide => return Ok(()),
    };
    std::process::Command::new(tool)
        .arg("--version")
        .current_dir(anodizer_core::path_util::probe_dir())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("blobs: '{tool}' CLI not found on PATH (required for client-side KMS encryption)"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                anyhow::bail!(
                    "blobs: '{tool} --version' exited {} — install or repair the CLI before running KMS-backed blob upload",
                    status.code().unwrap_or(-1)
                )
            }
        })
}

/// Run a CLI subprocess with stdin piped, return stdout on success.
/// Centralizes the spawn / write / wait_with_output pattern that the per-CLI
/// arms used to repeat with subtle differences in error wrapping.
///
/// `program` is one of `aws` / `gcloud` / `az` — the binary presence is
/// validated by [`preflight_kms_cli`] before any caller reaches this
/// helper, so a missing tool fails fast at upload-config validation
/// rather than per-artifact during fan-out.
pub(crate) fn run_kms_cli_with_stdin(
    program: &str,
    args: &[&str],
    stdin: &[u8],
    label: &str,
) -> Result<Vec<u8>> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("blobs: failed to spawn '{program}' for {label}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("blobs: {label}: child has no stdin"))?
        .write_all(stdin)
        .with_context(|| format!("blobs: {label}: failed to write plaintext to {program} stdin"))?;
    let output = child
        .wait_with_output()
        .with_context(|| format!("blobs: {label}: failed to wait for {program}"))?;
    if !output.status.success() {
        bail!(
            "blobs: {label} ({program}) failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// Encrypt `data` client-side using the appropriate cloud CLI tool.
///
/// Returns the encrypted ciphertext bytes. For `ServerSide`, returns the data
/// unchanged — the S3 builder handles SSE-KMS configuration at the transport
/// level.
pub(crate) fn encrypt_with_kms(
    data: &[u8],
    kms_key: &str,
    provider: KmsProvider,
) -> Result<Vec<u8>> {
    match provider {
        KmsProvider::Aws => {
            // awskms://key-id  or  awskms:///arn:aws:kms:region:account:key/id
            let key_id = kms_key
                .strip_prefix("awskms://")
                .ok_or_else(|| anyhow::anyhow!("expected awskms:// scheme, got {kms_key}"))?
                .trim_start_matches('/');
            let stdout = run_kms_cli_with_stdin(
                "aws",
                &[
                    "kms",
                    "encrypt",
                    "--key-id",
                    key_id,
                    "--plaintext",
                    "fileb:///dev/stdin",
                    "--output",
                    "json",
                ],
                data,
                "aws kms encrypt",
            )?;
            let resp: serde_json::Value = serde_json::from_slice(&stdout)
                .context("blobs: failed to parse aws kms encrypt JSON response")?;
            let b64 = resp["CiphertextBlob"]
                .as_str()
                .context("missing CiphertextBlob in aws kms encrypt response")?;
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .context("blobs: failed to decode CiphertextBlob base64")
        }

        KmsProvider::Gcp => {
            // gcpkms://projects/PROJECT/locations/LOC/keyRings/KR/cryptKeys/KEY
            let resource = kms_key
                .strip_prefix("gcpkms://")
                .ok_or_else(|| anyhow::anyhow!("expected gcpkms:// scheme, got {kms_key}"))?;
            // gcloud outputs raw ciphertext bytes to stdout.
            run_kms_cli_with_stdin(
                "gcloud",
                &[
                    "kms",
                    "encrypt",
                    "--key",
                    resource,
                    "--plaintext-file",
                    "-",
                    "--ciphertext-file",
                    "-",
                ],
                data,
                "gcloud kms encrypt",
            )
        }

        KmsProvider::Azure => {
            // azurekeyvault://vault-name/keys/key-name[/version]
            let path = kms_key.strip_prefix("azurekeyvault://").ok_or_else(|| {
                anyhow::anyhow!("expected azurekeyvault:// scheme, got {kms_key}")
            })?;
            let parts: Vec<&str> = path.splitn(3, '/').collect();
            let vault_name = parts
                .first()
                .context("missing vault name in azurekeyvault:// URL")?;
            // parts[1] is "keys", parts[2] is "key-name[/version]"
            let key_name = parts
                .get(2)
                .context("missing key name in azurekeyvault:// URL (expected vault/keys/name)")?;
            let b64_data = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data);
            // `az` reads --value, no stdin — use a one-shot Command::output here
            // because run_kms_cli_with_stdin assumes stdin-driven I/O.
            let output = std::process::Command::new("az")
                .args([
                    "keyvault",
                    "key",
                    "encrypt",
                    "--vault-name",
                    vault_name,
                    "--name",
                    key_name,
                    "--algorithm",
                    "RSA-OAEP-256",
                    "--value",
                    &b64_data,
                    "--output",
                    "json",
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .context("blobs: failed to spawn 'az' for keyvault encrypt")?;
            if !output.status.success() {
                bail!(
                    "blobs: az keyvault key encrypt failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            let resp: serde_json::Value = serde_json::from_slice(&output.stdout)
                .context("blobs: failed to parse az keyvault encrypt JSON response")?;
            let result = resp["result"]
                .as_str()
                .context("missing 'result' field in az keyvault encrypt response")?;
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(result)
                .context("blobs: failed to decode az keyvault encryption result")
        }

        KmsProvider::ServerSide => {
            // Not client-side encryption — return data unchanged.
            // The S3 builder handles SSE-KMS at the transport level.
            Ok(data.to_vec())
        }
    }
}
