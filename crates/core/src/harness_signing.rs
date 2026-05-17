//! Ephemeral signing-key provisioning for the determinism harness.
//!
//! Generates one cosign keypair (ECDSA P-256, the only algorithm the
//! current cosign CLI supports for `generate-key-pair`) and one GPG
//! keypair (EdDSA ed25519, deterministic under RFC 8032) inside a
//! temp dir whose lifetime is bound to [`EphemeralSigningKeys`]. The
//! harness consumes both via env vars (`COSIGN_KEY`, `GPG_FINGERPRINT`,
//! `GNUPGHOME`, ...) so the sign stage never sees host credentials.
//!
//! Cosign signatures are non-deterministic regardless of key reuse —
//! ECDSA random-`k` makes byte equality impossible. The harness's
//! `build_report` auto-allowlists `stage=sign` drift so that surface
//! doesn't fail the run; signature *verification* is the right gate
//! and belongs downstream of the harness.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};
use tempfile::TempDir;

/// Cosign requires a non-empty `generate-key-pair` password; signature
/// output is independent of the password (it only encrypts the on-disk
/// private key). Hardcoded so a single value is used across runs.
const HARNESS_COSIGN_PASSWORD: &str = "anodize-harness";

/// EdDSA ed25519 keypair config for `gpg --batch --gen-key`.
/// `%no-protection` skips the passphrase prompt; ed25519 + an SDE-pinned
/// signing time gives byte-stable detached signatures per RFC 8032.
const HARNESS_GPG_BATCH: &str = "%no-protection
Key-Type: EDDSA
Key-Curve: ed25519
Subkey-Type: EDDSA
Subkey-Curve: ed25519
Name-Real: Anodize Harness
Name-Email: harness@anodize.invalid
Expire-Date: 0
%commit
";

/// Ephemeral signing keys. Dropping the struct removes the temp dir and
/// all keys with it.
pub struct EphemeralSigningKeys {
    /// Encrypted cosign private-key contents (PEM). Read by
    /// `cosign sign-blob --key=env://COSIGN_KEY`.
    pub cosign_key_contents: String,
    /// Password for [`Self::cosign_key_contents`], set as
    /// `COSIGN_PASSWORD` env var.
    pub cosign_password: String,
    /// `GNUPGHOME` directory holding the ephemeral keyring.
    pub gnupg_home: PathBuf,
    /// GPG long key id (16 hex chars) for `gpg --local-user` — surfaced
    /// to user configs as `{{ Env.GPG_FINGERPRINT }}`.
    pub gpg_fingerprint: String,
    /// Path to the armored secret-key file (consumed by nfpm via the
    /// `GPG_KEY_PATH` env var when packaging signed deb/rpm/apk).
    pub gpg_key_path: PathBuf,
    /// Keeps the tmpdir alive for the harness's run loop. Dropped
    /// when [`EphemeralSigningKeys`] goes out of scope.
    _tmpdir: TempDir,
}

/// Provision ephemeral cosign + GPG keys. Returns `Err` when either
/// tool is missing or key generation fails — bailing is preferable to
/// silently skipping sign-stage validation.
pub fn provision_ephemeral_keys() -> Result<EphemeralSigningKeys> {
    let tmpdir = tempfile::Builder::new()
        .prefix("anodize-harness-signing-")
        .tempdir()
        .context("harness signing: create tempdir")?;

    let cosign_key_contents = provision_cosign(tmpdir.path())?;
    let (gnupg_home, gpg_fingerprint, gpg_key_path) = provision_gpg(tmpdir.path())?;

    Ok(EphemeralSigningKeys {
        cosign_key_contents,
        cosign_password: HARNESS_COSIGN_PASSWORD.into(),
        gnupg_home,
        gpg_fingerprint,
        gpg_key_path,
        _tmpdir: tmpdir,
    })
}

fn provision_cosign(tmpdir: &Path) -> Result<String> {
    if Command::new("cosign")
        .arg("version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        bail!(
            "harness signing: `cosign` not on PATH or failed to run. \
             Install cosign (e.g. via `anodizer-action` `install: cosign` \
             or system package manager) or drop `sign` from \
             `anodizer check determinism --stages=`."
        );
    }
    let prefix = "anodize-harness";
    let output = Command::new("cosign")
        .args(["generate-key-pair", "--output-key-prefix", prefix])
        .current_dir(tmpdir)
        .env("COSIGN_PASSWORD", HARNESS_COSIGN_PASSWORD)
        .output()
        .context("harness signing: spawn `cosign generate-key-pair`")?;
    if !output.status.success() {
        bail!(
            "harness signing: cosign generate-key-pair failed (exit {}):\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let key_path = tmpdir.join(format!("{prefix}.key"));
    let contents = std::fs::read_to_string(&key_path)
        .with_context(|| format!("harness signing: read cosign key at {}", key_path.display()))?;
    Ok(contents)
}

fn provision_gpg(tmpdir: &Path) -> Result<(PathBuf, String, PathBuf)> {
    if Command::new("gpg")
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        bail!(
            "harness signing: `gpg` not on PATH or failed to run. \
             Install GnuPG (e.g. apt-get install gnupg, brew install gpg, \
             choco install gnupg) or drop `sign` from \
             `anodizer check determinism --stages=`."
        );
    }

    let gnupg_home = tmpdir.join("gnupg");
    std::fs::create_dir(&gnupg_home).context("harness signing: create GNUPGHOME")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&gnupg_home)
            .context("harness signing: stat GNUPGHOME")?
            .permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&gnupg_home, perms)
            .context("harness signing: chmod 0700 GNUPGHOME")?;
    }

    let batch_path = tmpdir.join("gen-key.batch");
    std::fs::write(&batch_path, HARNESS_GPG_BATCH)
        .context("harness signing: write gpg batch-key-gen config")?;

    let gen_out = Command::new("gpg")
        .args(["--batch", "--gen-key"])
        .arg(&batch_path)
        .env("GNUPGHOME", &gnupg_home)
        .output()
        .context("harness signing: spawn `gpg --batch --gen-key`")?;
    if !gen_out.status.success() {
        bail!(
            "harness signing: gpg --gen-key failed (exit {}):\n{}\n{}",
            gen_out.status,
            String::from_utf8_lossy(&gen_out.stdout),
            String::from_utf8_lossy(&gen_out.stderr)
        );
    }

    let list_out = Command::new("gpg")
        .args(["--list-secret-keys", "--with-colons"])
        .env("GNUPGHOME", &gnupg_home)
        .output()
        .context("harness signing: spawn `gpg --list-secret-keys`")?;
    if !list_out.status.success() {
        bail!(
            "harness signing: gpg --list-secret-keys failed (exit {}):\n{}",
            list_out.status,
            String::from_utf8_lossy(&list_out.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&list_out.stdout);
    let fingerprint = parse_fingerprint(&stdout).ok_or_else(|| {
        anyhow::anyhow!(
            "harness signing: could not parse gpg --list-secret-keys --with-colons output:\n{}",
            stdout
        )
    })?;

    let gpg_key_path = tmpdir.join("anodize-harness.asc");
    let export_out = Command::new("gpg")
        .args(["--batch", "--armor", "--export-secret-keys", &fingerprint])
        .env("GNUPGHOME", &gnupg_home)
        .output()
        .context("harness signing: spawn `gpg --export-secret-keys`")?;
    if !export_out.status.success() {
        bail!(
            "harness signing: gpg --export-secret-keys failed (exit {}):\n{}",
            export_out.status,
            String::from_utf8_lossy(&export_out.stderr)
        );
    }
    std::fs::write(&gpg_key_path, &export_out.stdout).with_context(|| {
        format!(
            "harness signing: write exported secret key to {}",
            gpg_key_path.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&gpg_key_path)
            .context("harness signing: stat exported key")?
            .permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&gpg_key_path, perms)
            .context("harness signing: chmod 0600 exported key")?;
    }

    Ok((gnupg_home, fingerprint, gpg_key_path))
}

/// Pull the full fingerprint out of `gpg --list-secret-keys --with-colons`.
/// `fpr:` records carry the fingerprint in field 9 (0-indexed).
fn parse_fingerprint(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("fpr:") {
            let fields: Vec<&str> = rest.split(':').collect();
            if let Some(fpr) = fields.get(8)
                && fpr.len() >= 16
            {
                return Some(fpr.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fingerprint_extracts_full_fpr() {
        let sample = "sec:u:255:22:ABCDEF1234567890ABCDEF1234567890ABCDEF12:1730000000:::u:::scESC:::+:::ed25519:::0:\nfpr:::::::::ABCDEF1234567890ABCDEF1234567890ABCDEF12:\n";
        assert_eq!(
            parse_fingerprint(sample).as_deref(),
            Some("ABCDEF1234567890ABCDEF1234567890ABCDEF12")
        );
    }

    #[test]
    fn parse_fingerprint_none_when_no_fpr_record() {
        assert!(parse_fingerprint("sec:u:255:22::1730000000:::u::\n").is_none());
    }
}
