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
    /// `GPG_KEY_PATH` env var when packaging signed deb/rpm).
    pub gpg_key_path: PathBuf,
    /// Path to a PEM RSA private key consumed by nfpm's apk packager via
    /// the `APK_PRIVATE_KEY_PATH` env var. `None` when `openssl` is
    /// unavailable (apk then builds unsigned, as before).
    pub apk_key_path: Option<PathBuf>,
    /// Keeps the tmpdir alive for the harness's run loop. Dropped
    /// when [`EphemeralSigningKeys`] goes out of scope.
    _tmpdir: TempDir,
}

#[cfg(feature = "test-helpers")]
impl EphemeralSigningKeys {
    /// Construct an instance with caller-supplied field values for tests that
    /// exercise downstream env construction without spawning cosign/gpg/openssl.
    /// The backing tempdir is empty — the paths need not exist; consumers only
    /// read the fields.
    pub fn for_test(apk_key_path: Option<PathBuf>) -> Self {
        let tmpdir = tempfile::Builder::new()
            .prefix("agpg-test-")
            .tempdir()
            .expect("for_test: create tempdir");
        Self {
            cosign_key_contents: "TEST-COSIGN-KEY".into(),
            cosign_password: HARNESS_COSIGN_PASSWORD.into(),
            gnupg_home: tmpdir.path().join("gnupg"),
            gpg_fingerprint: "TESTFPR0000000000".into(),
            gpg_key_path: tmpdir.path().join("anodize-harness.asc"),
            apk_key_path,
            _tmpdir: tmpdir,
        }
    }
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
    let apk_key_path = provision_apk(tmpdir.path());

    Ok(EphemeralSigningKeys {
        cosign_key_contents,
        cosign_password: HARNESS_COSIGN_PASSWORD.into(),
        gnupg_home,
        gpg_fingerprint,
        gpg_key_path,
        apk_key_path,
        _tmpdir: tmpdir,
    })
}

/// Render `path` as the string we pass to gpg's subprocess env vars
/// (`GNUPGHOME`, `GPG_KEY_PATH`) on the host the harness runs on.
///
/// Windows ships gpg in two builds whose accepted path conventions do
/// not overlap, so a single static form cannot satisfy both:
///   * Git-for-Windows' MSYS2 gpg understands only a POSIX drive root
///     (`/c/Users/...`); a leading drive-letter colon (`C:`) is treated
///     as a filename, so gpg anchors it to its CWD and the path doesn't
///     exist.
///   * native Gpg4win understands only the drive-letter form
///     (`C:\` / `C:/`); a `/c/...` path is "No such file or directory".
///
/// We detect which build is on `PATH` ([`gpg_on_path_is_msys`], from
/// `gpg --version`'s `Home:` line) and emit the matching form. The
/// drive-letter forward-slash form is also what native cosign/openssl
/// accept, so it is the correct default for every non-MSYS case.
/// Non-Windows hosts pass the path through verbatim.
pub fn path_for_subprocess_env(path: &Path) -> String {
    let raw = path.to_string_lossy().into_owned();
    if !cfg!(windows) {
        return raw;
    }
    let forward = crate::util::normalize_path_separators(&raw);
    #[cfg(windows)]
    {
        if gpg_on_path_is_msys() {
            return to_msys_drive_form(&forward);
        }
    }
    forward
}

/// Rewrite a drive-letter forward-slash path (`C:/Users/x`) into the
/// MSYS2 drive-root form (`/c/Users/x`). Any path without a `<drive>:/`
/// prefix (already-POSIX, UNC, relative) is returned unchanged.
#[cfg(any(windows, test))]
fn to_msys_drive_form(forward: &str) -> String {
    let mut chars = forward.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(drive), Some(':'), Some('/')) if drive.is_ascii_alphabetic() => {
            format!("/{}/{}", drive.to_ascii_lowercase(), chars.as_str())
        }
        _ => forward.to_string(),
    }
}

/// Whether the `gpg` resolved on `PATH` is the MSYS2 (Git-for-Windows)
/// build, which needs POSIX `/c/...` paths rather than drive-letter
/// ones. Decided once and cached: `gpg --version` prints `Home: <dir>`
/// in the build's own convention — MSYS reports a POSIX root
/// (`/c/Users/...`), native Gpg4win reports a drive letter
/// (`C:\Users\...`). A gpg that is absent or whose output can't be
/// parsed defaults to "not MSYS" (the native/drive-letter form);
/// `provision_gpg`'s own `gpg --version` precheck is what surfaces a
/// genuinely missing gpg with an actionable error.
#[cfg(windows)]
fn gpg_on_path_is_msys() -> bool {
    use std::sync::OnceLock;
    static IS_MSYS: OnceLock<bool> = OnceLock::new();
    *IS_MSYS.get_or_init(|| {
        std::process::Command::new("gpg")
            .arg("--version")
            .output()
            .ok()
            .and_then(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("Home:"))
                    .map(|home| home.trim().starts_with('/'))
            })
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod path_tests {
    use super::{path_for_subprocess_env, to_msys_drive_form};
    use std::path::Path;

    #[test]
    fn unix_path_passes_through() {
        // Even compiled on a Windows host, /tmp-style input has no drive
        // letter to transform and returns unchanged content.
        let out = path_for_subprocess_env(Path::new("/tmp/foo/bar"));
        assert_eq!(out, "/tmp/foo/bar");
    }

    #[test]
    fn msys_drive_form_rewrites_drive_root() {
        // Pure transform (flavor-independent): drive-letter forward-slash
        // input becomes the MSYS2 `/c/...` root with a lowercased drive.
        assert_eq!(
            to_msys_drive_form("C:/Users/RUNNER~1/AppData/Local/Temp/agpg-x"),
            "/c/Users/RUNNER~1/AppData/Local/Temp/agpg-x"
        );
        assert_eq!(to_msys_drive_form("D:/foo"), "/d/foo");
    }

    #[test]
    fn msys_drive_form_passes_through_posix() {
        // No `<drive>:/` prefix => returned unchanged (native gpg + cosign
        // consume this drive-letter/POSIX form directly).
        assert_eq!(
            to_msys_drive_form("/already/posix/path"),
            "/already/posix/path"
        );
    }
}

fn provision_cosign(tmpdir: &Path) -> Result<String> {
    if Command::new("cosign")
        .arg("version")
        .current_dir(crate::path_util::probe_dir())
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

/// Generate an ephemeral RSA private key (PEM PKCS#8) for nfpm's apk
/// packager. nfpm RSA-signs apk; the signature is deterministic (no salt /
/// no embedded timestamp), so the signed apk is byte-reproducible.
///
/// Returns `None` when `openssl` is missing or key generation fails.
fn provision_apk(tmpdir: &Path) -> Option<PathBuf> {
    let key_path = tmpdir.join("apk-harness.pem");
    // Best-effort, NOT bail-on-failure (unlike provision_cosign/provision_gpg):
    // apk is a Linux-only format and not every determinism shard builds it, so
    // a missing `openssl` must NOT fail provisioning — bailing here would break
    // the macos/windows shards that pass today without any apk key.
    //
    // `genpkey` (not `genrsa`) is pinned deliberately so the key is always
    // PKCS#8 (`BEGIN PRIVATE KEY`) across openssl versions. `genrsa` emits
    // PKCS#1 on openssl 1.x/LibreSSL and PKCS#8 on 3.x; a format nfpm's apk
    // packager rejects would — because provisioning is best-effort — silently
    // leave apk unsigned in-harness, re-opening the signed-path blindspot.
    let ok = Command::new("openssl")
        .args([
            "genpkey",
            "-algorithm",
            "RSA",
            "-pkeyopt",
            "rsa_keygen_bits:2048",
            "-out",
        ])
        .arg(&key_path)
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&key_path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(&key_path, perms);
        }
    }
    Some(key_path)
}

fn provision_gpg(tmpdir: &Path, sde: i64) -> Result<(PathBuf, String, PathBuf)> {
    if Command::new("gpg")
        .arg("--version")
        .current_dir(crate::path_util::probe_dir())
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

    #[test]
    fn parse_fingerprint_skips_short_fpr_field() {
        assert!(parse_fingerprint("fpr:::::::::ABCD:\n").is_none());
    }
}

#[cfg(all(test, unix))]
mod provision_tests {
    use super::*;
    use crate::test_helpers::fake_tool::FakeToolDir;

    const FAKE_FPR: &str = "ABCDEF1234567890ABCDEF1234567890ABCDEF12";

    /// Run the provisioner expecting failure; returns the full anyhow chain.
    /// (A `Debug` bound on [`EphemeralSigningKeys`] is deliberately avoided —
    /// it would let the cosign private key reach debug logs.)
    fn provision_err(sde: i64) -> String {
        match provision_ephemeral_keys(sde) {
            Ok(_) => panic!("expected provisioning to fail"),
            Err(e) => format!("{e:#}"),
        }
    }

    /// `cosign` stub: succeeds on `version` and writes the keypair files
    /// (into its CWD — the provision tempdir) on `generate-key-pair`.
    fn stub_cosign_ok(tools: &FakeToolDir) {
        tools
            .tool("cosign")
            .script(
                "case \"$1\" in\n\
                 version) exit 0 ;;\n\
                 generate-key-pair)\n\
                   printf '%s' 'FAKE-ENCRYPTED-COSIGN-PEM' > cosign.key\n\
                   printf '%s' 'FAKE-COSIGN-PUB' > cosign.pub\n\
                   exit 0 ;;\n\
                 *) exit 1 ;;\n\
                 esac",
            )
            .install();
    }

    /// `openssl` stub: writes a fake PEM to the `-out` path and exits 0,
    /// so the apk provisioner's success path runs without a real openssl.
    fn stub_openssl_ok(tools: &FakeToolDir) {
        tools
            .tool("openssl")
            .script(
                "out=\"\"\n\
                 while [ $# -gt 0 ]; do\n\
                   if [ \"$1\" = '-out' ]; then out=\"$2\"; shift; fi\n\
                   shift\n\
                 done\n\
                 if [ -n \"$out\" ]; then printf '%s' '-----BEGIN PRIVATE KEY-----\\nFAKE\\n-----END PRIVATE KEY-----\\n' > \"$out\"; fi\n\
                 exit 0",
            )
            .install();
    }

    /// `gpg` stub covering the four argv shapes the provisioner issues:
    /// `--version`, `--batch --gen-key <file>`, `--list-secret-keys
    /// --with-colons`, `--batch --armor --export-secret-keys <fpr>`.
    fn stub_gpg_ok(tools: &FakeToolDir) {
        tools
            .tool("gpg")
            .script(format!(
                "if [ \"$1\" = '--version' ]; then exit 0; fi\n\
                 if [ \"$1\" = '--list-secret-keys' ]; then printf 'fpr:::::::::{FAKE_FPR}:\\n'; exit 0; fi\n\
                 if [ \"$2\" = '--gen-key' ]; then exit 0; fi\n\
                 if [ \"$2\" = '--armor' ]; then printf 'FAKE-ARMORED-SECRET-KEY\\n'; exit 0; fi\n\
                 exit 9"
            ))
            .install();
        tools.tool("gpgconf").install();
    }

    #[test]
    #[serial_test::serial]
    fn provision_happy_path_yields_keys_and_config() {
        let tools = FakeToolDir::new();
        stub_cosign_ok(&tools);
        stub_gpg_ok(&tools);
        stub_openssl_ok(&tools);
        let _g = tools.activate();

        // 2025-01-01T00:00:00Z
        let keys = provision_ephemeral_keys(1735689600).expect("provision succeeds");

        assert_eq!(keys.cosign_key_contents, "FAKE-ENCRYPTED-COSIGN-PEM");
        assert_eq!(keys.cosign_password, "anodize-harness");
        assert_eq!(keys.gpg_fingerprint, FAKE_FPR);
        let apk_key = keys
            .apk_key_path
            .as_ref()
            .expect("openssl stub yields an apk key path");
        assert!(apk_key.is_file(), "apk key file must exist");
        assert_eq!(
            std::fs::read_to_string(&keys.gpg_key_path).unwrap(),
            "FAKE-ARMORED-SECRET-KEY\n"
        );
        assert!(keys.gnupg_home.is_dir());
        assert_eq!(
            std::fs::read_to_string(keys.gnupg_home.join("gpg-agent.conf")).unwrap(),
            "allow-loopback-pinentry\n"
        );
        assert_eq!(
            std::fs::read_to_string(keys.gnupg_home.join("gpg.conf")).unwrap(),
            "pinentry-mode loopback\n"
        );
        {
            use std::os::unix::fs::PermissionsExt;
            let home_mode = std::fs::metadata(&keys.gnupg_home)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(home_mode & 0o777, 0o700, "GNUPGHOME must be 0700");
            let key_mode = std::fs::metadata(&keys.gpg_key_path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(key_mode & 0o777, 0o600, "exported key must be 0600");
        }

        let cosign_calls = tools.calls("cosign");
        assert_eq!(cosign_calls[0], vec!["version"]);
        assert_eq!(cosign_calls[1], vec!["generate-key-pair"]);

        let gpg_calls = tools.calls("gpg");
        assert_eq!(gpg_calls[0], vec!["--version"]);
        assert_eq!(gpg_calls[1][..2], ["--batch", "--gen-key"]);
        // The batch file handed to gen-key pins the key's creation time to
        // the SDE so faked-system-time signs see a usable key.
        let batch = std::fs::read_to_string(&gpg_calls[1][2]).unwrap();
        assert!(batch.contains("Creation-Date: 20250101T000000"), "{batch}");
        assert!(batch.contains("Key-Curve: ed25519"), "{batch}");
        assert!(batch.contains("%no-protection"), "{batch}");
        assert_eq!(gpg_calls[2], vec!["--list-secret-keys", "--with-colons"]);
        assert_eq!(
            gpg_calls[3],
            vec!["--batch", "--armor", "--export-secret-keys", FAKE_FPR]
        );
    }

    /// apk provisioning is best-effort: an unavailable `openssl` must NOT bail
    /// the whole provisioner (apk is Linux-only and not every shard builds it).
    /// Provisioning still succeeds and yields `apk_key_path == None`. A
    /// failing stub stands in for "openssl unavailable" — the fake PATH
    /// prepends but does not replace the host PATH, so a stub (shadowing any
    /// real host openssl) is required to drive the unavailable branch
    /// deterministically.
    #[test]
    #[serial_test::serial]
    fn provision_succeeds_without_openssl_yielding_no_apk_key() {
        let tools = FakeToolDir::new();
        stub_cosign_ok(&tools);
        stub_gpg_ok(&tools);
        tools.tool("openssl").exit(1).install();
        let _g = tools.activate();

        let keys = provision_ephemeral_keys(1735689600)
            .expect("provision must succeed even when openssl fails");
        assert!(
            keys.apk_key_path.is_none(),
            "apk_key_path must be None when openssl is unavailable"
        );
    }

    #[test]
    #[serial_test::serial]
    fn provision_bails_when_cosign_unavailable() {
        let tools = FakeToolDir::new();
        // `cosign version` failing is treated the same as cosign missing.
        tools.tool("cosign").exit(1).install();
        stub_gpg_ok(&tools);
        let _g = tools.activate();

        let err = provision_err(0);
        assert!(
            err.contains("`cosign` not on PATH or failed to run"),
            "{err}"
        );
        assert!(err.contains("Install cosign"), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn provision_bails_when_cosign_keygen_fails() {
        let tools = FakeToolDir::new();
        tools
            .tool("cosign")
            .script(
                "case \"$1\" in\n\
                 version) exit 0 ;;\n\
                 *) echo 'keygen exploded' 1>&2; exit 3 ;;\n\
                 esac",
            )
            .install();
        stub_gpg_ok(&tools);
        let _g = tools.activate();

        let err = provision_err(0);
        assert!(err.contains("cosign generate-key-pair failed"), "{err}");
        assert!(err.contains("keygen exploded"), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn provision_bails_when_cosign_key_file_missing() {
        let tools = FakeToolDir::new();
        // generate-key-pair "succeeds" but writes no cosign.key.
        tools.tool("cosign").install();
        stub_gpg_ok(&tools);
        let _g = tools.activate();

        let err = provision_err(0);
        assert!(err.contains("read cosign key"), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn provision_bails_when_gpg_unavailable() {
        let tools = FakeToolDir::new();
        stub_cosign_ok(&tools);
        tools.tool("gpg").exit(1).install();
        tools.tool("gpgconf").install();
        let _g = tools.activate();

        let err = provision_err(0);
        assert!(err.contains("`gpg` not on PATH or failed to run"), "{err}");
        assert!(err.contains("Install GnuPG"), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn provision_bails_on_out_of_range_sde() {
        let tools = FakeToolDir::new();
        stub_cosign_ok(&tools);
        stub_gpg_ok(&tools);
        let _g = tools.activate();

        let err = provision_err(i64::MAX);
        assert!(err.contains("out of range"), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn provision_bails_when_gen_key_fails() {
        let tools = FakeToolDir::new();
        stub_cosign_ok(&tools);
        tools
            .tool("gpg")
            .script(
                "if [ \"$1\" = '--version' ]; then exit 0; fi\n\
                 echo 'agent_genkey failed' 1>&2; exit 2",
            )
            .install();
        tools.tool("gpgconf").install();
        let _g = tools.activate();

        let err = provision_err(0);
        assert!(err.contains("gpg --gen-key failed"), "{err}");
        assert!(err.contains("agent_genkey failed"), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn provision_bails_when_list_secret_keys_fails() {
        let tools = FakeToolDir::new();
        stub_cosign_ok(&tools);
        tools
            .tool("gpg")
            .script(
                "if [ \"$1\" = '--version' ]; then exit 0; fi\n\
                 if [ \"$2\" = '--gen-key' ]; then exit 0; fi\n\
                 if [ \"$1\" = '--list-secret-keys' ]; then echo 'keyring locked' 1>&2; exit 2; fi\n\
                 exit 9",
            )
            .install();
        tools.tool("gpgconf").install();
        let _g = tools.activate();

        let err = provision_err(0);
        assert!(err.contains("gpg --list-secret-keys failed"), "{err}");
        assert!(err.contains("keyring locked"), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn provision_bails_when_fingerprint_unparseable() {
        let tools = FakeToolDir::new();
        stub_cosign_ok(&tools);
        tools
            .tool("gpg")
            .script(
                "if [ \"$1\" = '--version' ]; then exit 0; fi\n\
                 if [ \"$2\" = '--gen-key' ]; then exit 0; fi\n\
                 if [ \"$1\" = '--list-secret-keys' ]; then printf 'sec:u:255:22::0:::u::\\n'; exit 0; fi\n\
                 exit 9",
            )
            .install();
        tools.tool("gpgconf").install();
        let _g = tools.activate();

        let err = provision_err(0);
        assert!(
            err.contains("could not parse gpg --list-secret-keys"),
            "{err}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn provision_bails_when_export_fails() {
        let tools = FakeToolDir::new();
        stub_cosign_ok(&tools);
        tools
            .tool("gpg")
            .script(format!(
                "if [ \"$1\" = '--version' ]; then exit 0; fi\n\
                 if [ \"$2\" = '--gen-key' ]; then exit 0; fi\n\
                 if [ \"$1\" = '--list-secret-keys' ]; then printf 'fpr:::::::::{FAKE_FPR}:\\n'; exit 0; fi\n\
                 if [ \"$2\" = '--armor' ]; then echo 'export denied' 1>&2; exit 2; fi\n\
                 exit 9"
            ))
            .install();
        tools.tool("gpgconf").install();
        let _g = tools.activate();

        let err = provision_err(0);
        assert!(err.contains("gpg --export-secret-keys failed"), "{err}");
        assert!(err.contains("export denied"), "{err}");
    }
}
