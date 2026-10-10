use anyhow::{Context as _, Result};
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
    /// `gcpkms://projects/.../cryptoKeys/...` — client-side via gcloud CLI
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
    preflight_kms_cli_with_binary(std::path::Path::new(tool), tool)
}

/// Path-taking sibling of [`preflight_kms_cli`]: `binary` is what gets
/// spawned, `tool` the operator-facing CLI name for diagnostics.
/// Production passes `Path::new(<tool>)` (PATH lookup); tests point at
/// a nonexistent path to exercise the missing-CLI branch without
/// clobbering the process-wide `PATH` (which would make every
/// concurrent PATH-resolved spawn in the test binary flaky). Same injection point
/// convention as `stage-publish`'s `run_cargo_dry_run_with_binary`.
fn preflight_kms_cli_with_binary(binary: &std::path::Path, tool: &str) -> Result<()> {
    std::process::Command::new(binary)
        .arg("--version")
        .current_dir(anodizer_core::path_util::probe_dir())
        .stdin(std::process::Stdio::null())
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
    log: &anodizer_core::log::StageLogger,
) -> Result<Vec<u8>> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    // KMS output is raw/ciphertext bytes; never tee it live. The helper keys
    // its live-stream on the logger's verbosity, so route through a
    // Normal-verbosity clone (env preserved for redaction) so ciphertext is
    // captured silently regardless of the run's `--verbose` flag.
    let quiet_log = quiet_for_kms(log);
    let output = anodizer_core::run::run_checked_with_stdin(
        &mut cmd,
        stdin,
        &quiet_log,
        &format!("blobs: {label}"),
    )?;
    Ok(output.stdout)
}

/// A Normal-verbosity clone of `log` for the KMS encrypt subprocesses, so the
/// shared run helper captures (never streams) the raw ciphertext bytes the
/// cloud CLIs emit on stdout, while still carrying the original logger's env
/// for secret redaction of any stderr.
fn quiet_for_kms(log: &anodizer_core::log::StageLogger) -> anodizer_core::log::StageLogger {
    anodizer_core::log::StageLogger::new("blob", anodizer_core::log::Verbosity::Normal)
        .with_env(log.redaction_env())
}

/// The plaintext ceiling AWS KMS's direct `Encrypt` call accepts.
const AWS_KMS_MAX_PLAINTEXT: usize = 4096;

/// The plaintext ceiling Cloud KMS's `Encrypt` call accepts under a software
/// or external key: 64 KiB.
const GCP_KMS_MAX_PLAINTEXT: usize = 64 * 1024;

/// The plaintext ceiling Cloud KMS's `Encrypt` call accepts under an
/// HSM-protected key: 8 KiB. A file no larger than this fits every key.
const GCP_KMS_ANY_KEY_PLAINTEXT: usize = 8 * 1024;

/// The most `RSA-OAEP-256` encrypts under the largest key Key Vault issues,
/// 4096 bits: the modulus length less two SHA-256 digests and two bytes.
const AZURE_KEY_VAULT_MAX_PLAINTEXT: usize = 446;

/// The most `RSA-OAEP-256` encrypts under the smallest key Key Vault issues,
/// 2048 bits. A file no larger than this fits every key.
const AZURE_KEY_VAULT_ANY_KEY_PLAINTEXT: usize = 190;

/// Refuse a payload the key's KMS provider cannot encrypt in one call.
///
/// Each client-side scheme carries its own ceiling. A key spelled some other
/// way is left alone here — opening the keeper is what reports an unusable
/// key, and reporting it as a size problem would send the user to the wrong
/// place.
pub(crate) fn validate_kms_plaintext_size(kms_key: &str, size: usize) -> Result<()> {
    let (scheme, limit, note) = if kms_key.starts_with("awskms://") {
        ("awskms", AWS_KMS_MAX_PLAINTEXT, "")
    } else if kms_key.starts_with("gcpkms://") {
        (
            "gcpkms",
            GCP_KMS_MAX_PLAINTEXT,
            " (8192 under an HSM-protected key)",
        )
    } else if kms_key.starts_with("azurekeyvault://") {
        (
            "azurekeyvault",
            AZURE_KEY_VAULT_MAX_PLAINTEXT,
            " (RSA-OAEP-256 under a 4096-bit key; 318 under a 3072-bit key, 190 under a \
             2048-bit key)",
        )
    } else {
        return Ok(());
    };
    if size > limit {
        anyhow::bail!(
            "failed to encrypt with kms: {scheme} encryption supports files up to {limit} \
             bytes{note}, got {size} bytes",
        );
    }
    Ok(())
}

/// Encrypt `data` client-side using the appropriate cloud CLI tool.
///
/// Returns the encrypted ciphertext bytes. For `ServerSide`, returns the data
/// unchanged — the S3 builder handles SSE-KMS configuration at the transport
/// level.
/// The prefixes the `aws` CLI reads as "load this parameter from
/// elsewhere" rather than as the value itself: `file://` and `fileb://` read
/// a local file, `http://` and `https://` fetch a URL.
const AWS_CLI_INDIRECT_PREFIXES: [&str; 4] = ["file://", "fileb://", "http://", "https://"];

/// Refuse a `kms_key` component that the provider's CLI would read as
/// something other than the value: a leading `-` is a flag to every CLI, and
/// `indirect` names the prefixes this CLI dereferences.
///
/// Each of these components is passed on the command line as written, so a
/// `kms_key` holding one would make `aws kms encrypt` read a file or fetch
/// a URL on the runner, or make any of the three CLIs parse an option.
fn reject_cli_indirection(value: &str, what: &str, indirect: &[&str]) -> Result<()> {
    anyhow::ensure!(
        !value.starts_with('-'),
        "blobs: the {what} in kms_key starts with '-', which the CLI reads as an option: {value}"
    );
    if let Some(prefix) = indirect.iter().find(|p| {
        value
            .get(..p.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(p))
    }) {
        anyhow::bail!(
            "blobs: the {what} in kms_key starts with '{prefix}', which the CLI reads as a \
             file or URL to load the value from: {value}"
        );
    }
    Ok(())
}

pub(crate) fn encrypt_with_kms(
    data: &[u8],
    kms_key: &str,
    provider: KmsProvider,
    log: &anodizer_core::log::StageLogger,
) -> Result<Vec<u8>> {
    // Checked before the scheme dispatch so an oversized payload never leaves
    // the machine: the provider would reject it only after being handed the
    // plaintext.
    validate_kms_plaintext_size(kms_key, data.len())?;
    match provider {
        KmsProvider::Aws => {
            // awskms://key-id  or  awskms:///arn:aws:kms:region:account:key/id
            let key_id = kms_key
                .strip_prefix("awskms://")
                .ok_or_else(|| anyhow::anyhow!("expected awskms:// scheme, got {kms_key}"))?
                .trim_start_matches('/');
            reject_cli_indirection(key_id, "key id", &AWS_CLI_INDIRECT_PREFIXES)?;
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
                log,
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
            // gcpkms://projects/PROJECT/locations/LOC/keyRings/KR/cryptoKeys/KEY
            let resource = kms_key
                .strip_prefix("gcpkms://")
                .ok_or_else(|| anyhow::anyhow!("expected gcpkms:// scheme, got {kms_key}"))?;
            reject_cli_indirection(resource, "key resource", &[])?;
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
                log,
            )
            .map_err(|e| {
                // The key's protection level is not known here, and a file
                // past the HSM ceiling fails for that reason on an HSM key.
                if data.len() > GCP_KMS_ANY_KEY_PLAINTEXT {
                    e.context(format!(
                        "blobs: Cloud KMS encrypts at most 8192 bytes under an HSM-protected \
                         key; this file is {} bytes, so the key's protection level may be \
                         what refused it",
                        data.len()
                    ))
                } else {
                    e
                }
            })
        }

        KmsProvider::Azure => {
            // azurekeyvault://vault-name/keys/key-name[/version]
            let path = kms_key.strip_prefix("azurekeyvault://").ok_or_else(|| {
                anyhow::anyhow!("expected azurekeyvault:// scheme, got {kms_key}")
            })?;
            let parts: Vec<&str> = path.splitn(3, '/').collect();
            let vault_name = parts
                .first()
                .filter(|v| !v.is_empty())
                .context("missing vault name in azurekeyvault:// URL (expected vault/keys/name)")?;
            anyhow::ensure!(
                parts.get(1) == Some(&"keys"),
                "blobs: expected azurekeyvault://<vault>/keys/<name>[/<version>], got {kms_key}"
            );
            let (key_name, key_version) = parts
                .get(2)
                .map(|name| match name.split_once('/') {
                    Some((name, version)) => (name, Some(version)),
                    None => (*name, None),
                })
                .filter(|(name, _)| !name.is_empty())
                .context("missing key name in azurekeyvault:// URL (expected vault/keys/name)")?;
            reject_cli_indirection(vault_name, "vault name", &[])?;
            reject_cli_indirection(key_name, "key name", &[])?;
            if let Some(version) = key_version {
                reject_cli_indirection(version, "key version", &[])?;
            }
            // `az` decodes --value and encodes its result with the standard
            // alphabet and padding; the URL-safe one is refused or misread.
            let b64_data = base64::engine::general_purpose::STANDARD.encode(data);
            // `az keyvault key encrypt` takes its input only as `--value`
            // (no file or stdin form, checked against the az reference), so
            // the plaintext is on the command line for the call's duration;
            // the docs page says so. Like the stdin arms, ciphertext is
            // captured silently (quiet logger) rather than teed at verbose.
            let mut cmd = std::process::Command::new("az");
            cmd.args([
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
            ]);
            if let Some(version) = key_version.filter(|v| !v.is_empty()) {
                cmd.args(["--version", version]);
            }
            let quiet_log = quiet_for_kms(log);
            let output = anodizer_core::run::run_checked(
                &mut cmd,
                &quiet_log,
                "blobs: az keyvault key encrypt",
            )
            .map_err(|e| {
                // Which key size the vault holds is not known here, and a
                // file past the smallest key's ceiling fails for that reason
                // on a key that small.
                if data.len() > AZURE_KEY_VAULT_ANY_KEY_PLAINTEXT {
                    e.context(format!(
                        "blobs: RSA-OAEP-256 encrypts at most 190 bytes under a 2048-bit key \
                         and 318 under a 3072-bit key; this file is {} bytes, so the key may \
                         be too small for it",
                        data.len()
                    ))
                } else {
                    e
                }
            })?;
            let resp: serde_json::Value = serde_json::from_slice(&output.stdout)
                .context("blobs: failed to parse az keyvault encrypt JSON response")?;
            let result = resp["result"]
                .as_str()
                .context("missing 'result' field in az keyvault encrypt response")?;
            base64::engine::general_purpose::STANDARD
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

#[cfg(test)]
mod tests {
    use super::*;
    // Used only by the unix-gated PATH-stub tests below; the gate must match
    // or the import reads as unused on a Windows build.
    #[cfg(unix)]
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;

    /// A quiet test logger for the KMS run-helper calls. Verbosity is
    /// irrelevant to these tests (the helper captures ciphertext silently);
    /// a default logger keeps the call sites terse.
    fn tlog() -> anodizer_core::log::StageLogger {
        anodizer_core::log::StageLogger::new("blob-test", anodizer_core::log::Verbosity::Normal)
    }

    // -------------------------------------------------------------------
    // validate_kms_provider_match — scheme↔provider compatibility gate.
    // Accepted: S3↔Aws, S3↔ServerSide, Gcs↔Gcp, AzBlob↔Azure. Every other
    // pairing must bail with a message naming the expected scheme so the
    // misconfig surfaces at config-validate, not deep in the upload phase.
    // -------------------------------------------------------------------

    #[test]
    fn validate_match_accepts_each_native_pairing() {
        assert!(
            validate_kms_provider_match(Provider::S3, KmsProvider::Aws, "awskms://k").is_ok(),
            "s3 + awskms:// is the native AWS client-side pairing"
        );
        assert!(
            validate_kms_provider_match(Provider::S3, KmsProvider::ServerSide, "arn:aws:...")
                .is_ok(),
            "s3 + plain ARN is SSE-KMS server-side, accepted on S3"
        );
        assert!(
            validate_kms_provider_match(Provider::Gcs, KmsProvider::Gcp, "gcpkms://k").is_ok(),
            "gcs + gcpkms:// is the native GCP pairing"
        );
        assert!(
            validate_kms_provider_match(Provider::AzBlob, KmsProvider::Azure, "azurekeyvault://k")
                .is_ok(),
            "azblob + azurekeyvault:// is the native Azure pairing"
        );
    }

    #[test]
    fn validate_match_rejects_serverside_on_non_s3() {
        // Plain ARN (ServerSide) is only honored on S3 — GCS/Azure object_store
        // backends don't surface SSE-KMS through canned headers.
        let err = validate_kms_provider_match(Provider::Gcs, KmsProvider::ServerSide, "plain-key")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not compatible") && err.contains("gcpkms://"),
            "GCS + plain ARN must reject and name the expected gcpkms:// scheme; got: {err}"
        );
    }

    #[test]
    fn validate_match_rejects_cross_cloud_scheme() {
        // awskms:// against a GCS bucket: wrong cloud entirely. Error must
        // echo the offending key and the expected scheme for the provider.
        let err = validate_kms_provider_match(Provider::Gcs, KmsProvider::Aws, "awskms://oops")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("awskms://oops"),
            "error should echo the offending kms_key; got: {err}"
        );
        assert!(
            err.contains("gcpkms://"),
            "error should name the scheme the provider expects; got: {err}"
        );

        let err = validate_kms_provider_match(Provider::AzBlob, KmsProvider::Gcp, "gcpkms://k")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("azurekeyvault://"),
            "azblob mismatch must name azurekeyvault://; got: {err}"
        );
    }

    // -------------------------------------------------------------------
    // preflight_kms_cli — ServerSide is a no-op (no CLI needed); a missing
    // client-side CLI must surface as an error before fan-out.
    // -------------------------------------------------------------------

    #[test]
    fn preflight_serverside_is_noop() {
        // ServerSide returns Ok without probing any binary — SSE-KMS happens
        // at the S3 transport level, no CLI to verify.
        assert!(preflight_kms_cli(KmsProvider::ServerSide).is_ok());
    }

    #[test]
    fn preflight_errors_when_cli_missing() {
        // A nonexistent binary path exercises the spawn-failure branch
        // and must surface the "not found on PATH" context. Driven
        // through the binary-path override: replacing the process-wide PATH
        // instead would make every concurrent PATH-resolved spawn in
        // this test binary flaky.
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let missing = tmp.path().join("nonexistent-aws");

        let err = preflight_kms_cli_with_binary(&missing, "aws")
            .expect_err("missing aws CLI must surface as an error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("aws") && msg.contains("PATH"),
            "missing-CLI error must name the tool and PATH; got: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn preflight_errors_on_nonzero_version_exit() {
        // A present-but-broken CLI (exits nonzero on --version) must also be
        // rejected — repaired/installed wording, not the not-found wording.
        let tools = FakeToolDir::new();
        tools.tool("gcloud").exit(3).install();
        let _guard = tools.activate();
        let err = preflight_kms_cli(KmsProvider::Gcp)
            .expect_err("gcloud --version exit 3 must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("gcloud") && msg.contains("exited"),
            "nonzero-version error must name the tool and the bad exit; got: {msg}"
        );
    }

    // -------------------------------------------------------------------
    // run_kms_cli_with_stdin — central spawn/write/wait helper. Asserts the
    // exact argv reaches the tool, stdin is piped through, stdout is
    // returned on success, and a nonzero exit surfaces stderr.
    // -------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn run_kms_cli_passes_argv_and_returns_stdout() {
        let tools = FakeToolDir::new();
        tools.tool("aws").stdout("SIGNED-OUTPUT").install();
        let _guard = tools.activate();

        let out = run_kms_cli_with_stdin(
            "aws",
            &["kms", "encrypt", "--key-id", "abc"],
            b"plaintext",
            "aws kms encrypt",
            &tlog(),
        )
        .expect("stubbed aws exits 0");
        assert_eq!(out, b"SIGNED-OUTPUT", "stdout of the CLI must be returned");

        let calls = tools.calls("aws");
        assert_eq!(calls.len(), 1, "exactly one spawn");
        assert_eq!(
            calls[0],
            vec!["kms", "encrypt", "--key-id", "abc"],
            "argv must reach the tool verbatim and in order"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn run_kms_cli_surfaces_stderr_on_failure() {
        let tools = FakeToolDir::new();
        tools
            .tool("gcloud")
            .stderr("PERMISSION_DENIED: no kms access")
            .exit(1)
            .install();
        let _guard = tools.activate();

        let err = run_kms_cli_with_stdin(
            "gcloud",
            &["kms", "encrypt"],
            b"x",
            "gcloud kms encrypt",
            &tlog(),
        )
        .expect_err("nonzero exit must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("PERMISSION_DENIED") && msg.contains("gcloud kms encrypt"),
            "failure must surface the CLI stderr and the label; got: {msg}"
        );
    }

    // -------------------------------------------------------------------
    // encrypt_with_kms — per-provider client-side encryption. Each arm
    // strips the scheme, builds the right argv, and decodes the response.
    // Stubs emit canned tool output; the assertions cover the argv AND the
    // decoded ciphertext.
    // -------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn encrypt_aws_strips_scheme_decodes_b64_ciphertext() {
        // aws kms encrypt returns JSON {"CiphertextBlob": "<base64>"}; the
        // helper must parse it and base64-decode to raw bytes. The key id
        // passed to --key-id must have the awskms:// scheme (and leading
        // slashes) stripped.
        let secret = b"\x01\x02\x03ciphertext";
        let b64 = base64::engine::general_purpose::STANDARD.encode(secret);
        let json = format!("{{\"CiphertextBlob\":\"{b64}\"}}");

        let tools = FakeToolDir::new();
        tools.tool("aws").stdout(json).install();
        let _guard = tools.activate();

        let out = encrypt_with_kms(
            b"plaintext-data",
            "awskms:///arn:aws:kms:us-east-1:1:key/abc",
            KmsProvider::Aws,
            &tlog(),
        )
        .expect("aws encrypt happy path");
        assert_eq!(
            out, secret,
            "decoded CiphertextBlob must be returned as raw bytes"
        );

        let argv = &tools.calls("aws")[0];
        assert_eq!(&argv[0..2], &["kms", "encrypt"], "kms encrypt subcommand");
        let kid = argv_value(argv, "--key-id");
        assert_eq!(
            kid, "arn:aws:kms:us-east-1:1:key/abc",
            "--key-id must have awskms:// scheme and leading slashes stripped"
        );
        assert_eq!(
            argv_value(argv, "--plaintext"),
            "fileb:///dev/stdin",
            "plaintext is fed via fileb:///dev/stdin (the piped stdin)"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn encrypt_aws_errors_on_malformed_json() {
        // Non-JSON tool output must produce a parse error, not a panic.
        let tools = FakeToolDir::new();
        tools.tool("aws").stdout("not json at all").install();
        let _guard = tools.activate();

        let err = encrypt_with_kms(b"data", "awskms://k", KmsProvider::Aws, &tlog())
            .expect_err("malformed JSON must error");
        assert!(
            format!("{err:#}").contains("parse aws kms encrypt JSON"),
            "error must name the JSON parse failure; got: {err:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn encrypt_aws_errors_when_ciphertext_field_missing() {
        // Valid JSON but no CiphertextBlob field — the contract requires it.
        let tools = FakeToolDir::new();
        tools.tool("aws").stdout("{\"Other\":\"x\"}").install();
        let _guard = tools.activate();

        let err = encrypt_with_kms(b"data", "awskms://k", KmsProvider::Aws, &tlog())
            .expect_err("missing CiphertextBlob must error");
        assert!(
            format!("{err:#}").contains("CiphertextBlob"),
            "error must name the missing field; got: {err:#}"
        );
    }

    #[test]
    fn encrypt_aws_rejects_wrong_scheme() {
        // encrypt_with_kms(Aws, ...) requires the awskms:// prefix on the key.
        // A bare key with KmsProvider::Aws is a programming error and must
        // bail before any spawn.
        let err = encrypt_with_kms(b"data", "plain-key", KmsProvider::Aws, &tlog())
            .expect_err("Aws arm requires awskms:// scheme");
        assert!(
            format!("{err:#}").contains("awskms://"),
            "error must name the expected scheme; got: {err:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn encrypt_gcp_strips_scheme_passes_resource_returns_raw_stdout() {
        // gcloud writes raw ciphertext bytes straight to stdout; the helper
        // returns them unchanged. --key must receive the gcpkms://-stripped
        // resource path, and the stdin/stdout files must be "-".
        let raw = "RAW-GCLOUD-CIPHERTEXT";
        let tools = FakeToolDir::new();
        tools.tool("gcloud").stdout(raw).install();
        let _guard = tools.activate();

        let resource = "projects/p/locations/global/keyRings/kr/cryptoKeys/k";
        let out = encrypt_with_kms(
            b"plaintext",
            &format!("gcpkms://{resource}"),
            KmsProvider::Gcp,
            &tlog(),
        )
        .expect("gcp encrypt happy path");
        assert_eq!(
            out,
            raw.as_bytes(),
            "gcloud raw stdout is the ciphertext, returned unchanged"
        );

        let argv = &tools.calls("gcloud")[0];
        assert_eq!(&argv[0..2], &["kms", "encrypt"], "kms encrypt subcommand");
        assert_eq!(
            argv_value(argv, "--key"),
            resource,
            "--key must be the gcpkms://-stripped resource path"
        );
        assert_eq!(
            argv_value(argv, "--plaintext-file"),
            "-",
            "plaintext read from stdin"
        );
        assert_eq!(
            argv_value(argv, "--ciphertext-file"),
            "-",
            "ciphertext written to stdout"
        );
    }

    #[test]
    fn encrypt_gcp_rejects_wrong_scheme() {
        let err = encrypt_with_kms(b"data", "awskms://k", KmsProvider::Gcp, &tlog())
            .expect_err("Gcp arm requires gcpkms:// scheme");
        assert!(
            format!("{err:#}").contains("gcpkms://"),
            "error must name the expected scheme; got: {err:#}"
        );
    }

    /// Every component of a `kms_key` that reaches a CLI argument is refused
    /// when the CLI would read it as an option, a file or a URL, before any
    /// CLI is spawned.
    #[test]
    fn kms_key_components_the_cli_would_dereference_are_refused() {
        let refused = |kms_key: &str, provider: KmsProvider| {
            format!(
                "{:#}",
                encrypt_with_kms(b"x", kms_key, provider, &tlog()).unwrap_err()
            )
        };
        for (kms_key, expected) in [
            (
                "awskms://file:///etc/passwd",
                "the key id in kms_key starts with 'file://', which the CLI reads as a file or \
                 URL to load the value from: file:///etc/passwd",
            ),
            (
                "awskms://fileb:///dev/stdin",
                "the key id in kms_key starts with 'fileb://'",
            ),
            (
                "awskms://http://169.254.169.254/",
                "the key id in kms_key starts with 'http://'",
            ),
            (
                "awskms://HTTPS://example.com/key",
                "the key id in kms_key starts with 'https://'",
            ),
            (
                "awskms://--profile",
                "the key id in kms_key starts with '-', which the CLI reads as an option: --profile",
            ),
        ] {
            let msg = refused(kms_key, KmsProvider::Aws);
            assert!(msg.contains(expected), "{kms_key}: {msg}");
        }
        let msg = refused("gcpkms://--key-ring=x", KmsProvider::Gcp);
        assert!(
            msg.contains("the key resource in kms_key starts with '-'"),
            "{msg}"
        );
        for (kms_key, expected) in [
            (
                "azurekeyvault://-v/keys/k",
                "the vault name in kms_key starts with '-'",
            ),
            (
                "azurekeyvault://v/keys/-k",
                "the key name in kms_key starts with '-'",
            ),
            (
                "azurekeyvault://v/keys/k/--id",
                "the key version in kms_key starts with '-'",
            ),
            (
                "azurekeyvault:///keys/k",
                "missing vault name in azurekeyvault:// URL",
            ),
            (
                "azurekeyvault://v/keys/",
                "missing key name in azurekeyvault:// URL",
            ),
            (
                "azurekeyvault://v/secrets/k",
                "expected azurekeyvault://<vault>/keys/<name>[/<version>], got \
                 azurekeyvault://v/secrets/k",
            ),
        ] {
            let msg = refused(kms_key, KmsProvider::Azure);
            assert!(msg.contains(expected), "{kms_key}: {msg}");
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn encrypt_azure_parses_vault_and_key_builds_argv() {
        // azurekeyvault://VAULT/keys/NAME[/version] — the helper splits the
        // path, base64-encodes the plaintext into --value, and decodes the
        // {"result":"<base64>"} response, both in the standard padded
        // alphabet `az` itself uses. The bytes here encode to `+`, `/` and
        // `=`, which the URL-safe alphabet spells differently.
        let secret = b"\xfb\xff\xbeazure-ciphertext";
        let result_b64 = base64::engine::general_purpose::STANDARD.encode(secret);
        assert!(result_b64.contains('+') && result_b64.contains('/') && result_b64.ends_with('='));
        let json = format!("{{\"result\":\"{result_b64}\"}}");

        let tools = FakeToolDir::new();
        tools.tool("az").stdout(json).install();
        let _guard = tools.activate();

        let plaintext = b"\xfb\xff\xbesensitive";
        let out = encrypt_with_kms(
            plaintext,
            "azurekeyvault://my-vault/keys/my-key/v2",
            KmsProvider::Azure,
            &tlog(),
        )
        .expect("azure encrypt happy path");
        assert_eq!(
            out, secret,
            "the decoded result must be the returned ciphertext"
        );

        let argv = &tools.calls("az")[0];
        assert_eq!(
            &argv[0..3],
            &["keyvault", "key", "encrypt"],
            "az keyvault key encrypt subcommand"
        );
        assert_eq!(
            argv_value(argv, "--vault-name"),
            "my-vault",
            "vault name parsed from the URL host segment"
        );
        assert_eq!(
            argv_value(argv, "--name"),
            "my-key",
            "key name is the segment after vault/keys/"
        );
        assert_eq!(
            argv_value(argv, "--version"),
            "v2",
            "a trailing segment is the key version"
        );
        assert_eq!(
            argv_value(argv, "--value"),
            base64::engine::general_purpose::STANDARD.encode(plaintext),
            "plaintext is base64-encoded into --value (az has no stdin path)"
        );
        assert_eq!(argv_value(argv, "--algorithm"), "RSA-OAEP-256");
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn encrypt_azure_surfaces_cli_failure_stderr() {
        let tools = FakeToolDir::new();
        tools
            .tool("az")
            .stderr("Forbidden: key not found")
            .exit(1)
            .install();
        let _guard = tools.activate();

        let err = encrypt_with_kms(
            b"data",
            "azurekeyvault://v/keys/k",
            KmsProvider::Azure,
            &tlog(),
        )
        .expect_err("az nonzero exit must error");
        assert!(
            format!("{err:#}").contains("Forbidden: key not found"),
            "az failure must surface the CLI stderr; got: {err:#}"
        );
    }

    #[test]
    fn encrypt_azure_rejects_wrong_scheme() {
        let err = encrypt_with_kms(b"data", "gcpkms://k", KmsProvider::Azure, &tlog())
            .expect_err("Azure arm requires azurekeyvault:// scheme");
        assert!(
            format!("{err:#}").contains("azurekeyvault://"),
            "error must name the expected scheme; got: {err:#}"
        );
    }

    // -- test helpers --------------------------------------------------

    /// Return the argv element immediately following `flag`.
    #[cfg(unix)]
    fn argv_value(argv: &[String], flag: &str) -> String {
        let i = argv
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("flag {flag} not in argv {argv:?}"));
        argv.get(i + 1)
            .unwrap_or_else(|| panic!("no value after {flag} in {argv:?}"))
            .clone()
    }
}
