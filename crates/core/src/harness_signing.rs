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
///
/// Security note: this password is for the ephemeral test-harness-only
/// keypair generated inside a per-run tempdir. It never gates a
/// production signing key; it only encrypts the throwaway private key
/// in memory while the harness drives a hermetic rebuild. The cosign
/// signature itself is non-deterministic regardless (ECDSA random-k),
/// so verifying the harness output downstream is the real gate.
const HARNESS_COSIGN_PASSWORD: &str = "anodize-harness";

/// EdDSA ed25519 keypair config template for `gpg --batch --gen-key`.
/// `%no-protection` skips the passphrase prompt; ed25519 + an SDE-pinned
/// signing time gives byte-stable detached signatures per RFC 8032.
/// `Creation-Date: {creation}` is filled in at provision time so the
/// key's creation timestamp matches the harness's pinned epoch — sign
/// calls that use `--faked-system-time=<sde>` would otherwise see the
/// key as "not yet existing at that time" and bail with `Unusable
/// secret key`.
const HARNESS_GPG_BATCH_TEMPLATE: &str = "%no-protection
Key-Type: EDDSA
Key-Curve: ed25519
Subkey-Type: EDDSA
Subkey-Curve: ed25519
Name-Real: Anodize Harness
Name-Email: harness@anodize.invalid
Creation-Date: {creation}
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

/// Provision ephemeral cosign + GPG keys. `sde` is the harness's
/// pinned `SOURCE_DATE_EPOCH`; the GPG key's creation timestamp is
/// pinned to it so subsequent signs at the same epoch can use the key.
/// Returns `Err` when either tool is missing or key generation fails —
/// bailing is preferable to silently skipping sign-stage validation.
pub fn provision_ephemeral_keys(sde: i64) -> Result<EphemeralSigningKeys> {
    // GNUPGHOME root must stay SHORT — macOS Unix-domain socket paths
    // are capped at 104 chars (`sun_path`), and gpg-agent's
    // `S.gpg-agent.extra` socket name eats ~18 of those. The system
    // temp dir on macOS is `/var/folders/<hash>/T/` (~50 chars) which
    // leaves no room. `/tmp` is universally short on Linux + macOS;
    // Windows uses named pipes (no socket length limit) so the
    // system temp dir is fine there.
    let root: PathBuf = if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let tmpdir = tempfile::Builder::new()
        .prefix("agpg-")
        .tempdir_in(&root)
        .context("harness signing: create tempdir")?;

    let cosign_key_contents = provision_cosign(tmpdir.path())?;
    let (gnupg_home, gpg_fingerprint, gpg_key_path) = provision_gpg(tmpdir.path(), sde)?;

    Ok(EphemeralSigningKeys {
        cosign_key_contents,
        cosign_password: HARNESS_COSIGN_PASSWORD.into(),
        gnupg_home,
        gpg_fingerprint,
        gpg_key_path,
        _tmpdir: tmpdir,
    })
}

/// Render `path` as the string we pass to subprocess env vars. The
/// runner's gpg on Windows is Git-for-Windows' MSYS2 build, which
/// treats a leading drive-letter colon (`C:`) as a filename — it does
/// not anchor an absolute path — so gpg prepends its CWD and the
/// resulting path doesn't exist. MSYS expects `/c/...` instead. The
/// backslash-separator misparse is the same root cause.
pub fn path_for_subprocess_env(path: &Path) -> String {
    let raw = path.to_string_lossy().into_owned();
    if !cfg!(windows) {
        return raw;
    }
    let forward = crate::util::normalize_path_separators(&raw);
    let mut chars = forward.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(drive), Some(':'), Some('/')) if drive.is_ascii_alphabetic() => {
            format!("/{}/{}", drive.to_ascii_lowercase(), chars.as_str())
        }
        _ => forward,
    }
}

#[cfg(test)]
mod path_tests {
    use super::path_for_subprocess_env;
    use std::path::Path;

    #[test]
    fn unix_path_passes_through() {
        // Even compiled on Windows host, /tmp-style input has no drive
        // letter to transform and returns unchanged content.
        let out = path_for_subprocess_env(Path::new("/tmp/foo/bar"));
        assert_eq!(out, "/tmp/foo/bar");
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_colon_becomes_msys_root() {
        let out =
            path_for_subprocess_env(Path::new(r"C:\Users\RUNNER~1\AppData\Local\Temp\agpg-x"));
        assert_eq!(out, "/c/Users/RUNNER~1/AppData/Local/Temp/agpg-x");
    }

    #[cfg(windows)]
    #[test]
    fn windows_lowercases_drive_letter() {
        let out = path_for_subprocess_env(Path::new(r"D:\foo"));
        assert_eq!(out, "/d/foo");
    }
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
    // Use cosign's default `cosign.key` / `cosign.pub` output names —
    // `--output-key-prefix` only exists in cosign 2.x, and chocolatey
    // ships an older version on Windows runners.
    let output = Command::new("cosign")
        .args(["generate-key-pair"])
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
    let key_path = tmpdir.join("cosign.key");
    let contents = std::fs::read_to_string(&key_path)
        .with_context(|| format!("harness signing: read cosign key at {}", key_path.display()))?;
    Ok(contents)
}

fn provision_gpg(tmpdir: &Path, sde: i64) -> Result<(PathBuf, String, PathBuf)> {
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

    // Pre-write gpg-agent + gpg config so the agent that gets spawned
    // for keygen accepts non-interactive operation. Without this, fresh
    // GNUPGHOMEs on macOS runners fail with `agent_genkey failed: No
    // agent running` because the auto-spawned agent can't establish its
    // IPC socket without the loopback-pinentry pragma.
    std::fs::write(
        gnupg_home.join("gpg-agent.conf"),
        "allow-loopback-pinentry\n",
    )
    .context("harness signing: write gpg-agent.conf")?;
    std::fs::write(gnupg_home.join("gpg.conf"), "pinentry-mode loopback\n")
        .context("harness signing: write gpg.conf")?;

    // Explicitly launch the agent before keygen so the socket exists
    // by the time `gpg --gen-key` attempts to talk to it.
    let _ = Command::new("gpgconf")
        .args(["--launch", "gpg-agent"])
        .env("GNUPGHOME", path_for_subprocess_env(&gnupg_home))
        .output();

    let creation_dt = chrono::DateTime::<chrono::Utc>::from_timestamp(sde, 0)
        .ok_or_else(|| anyhow::anyhow!("harness signing: SDE {} out of range", sde))?;
    let creation_str = creation_dt.format("%Y%m%dT%H%M%S").to_string();
    let batch_path = tmpdir.join("gen-key.batch");
    let batch_config = HARNESS_GPG_BATCH_TEMPLATE.replace("{creation}", &creation_str);
    std::fs::write(&batch_path, &batch_config)
        .context("harness signing: write gpg batch-key-gen config")?;

    let gen_out = Command::new("gpg")
        .args(["--batch", "--gen-key"])
        .arg(path_for_subprocess_env(&batch_path))
        .env("GNUPGHOME", path_for_subprocess_env(&gnupg_home))
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
        .env("GNUPGHOME", path_for_subprocess_env(&gnupg_home))
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
        .env("GNUPGHOME", path_for_subprocess_env(&gnupg_home))
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
