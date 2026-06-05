//! Install smoke-test: install a Linux package in a pinned container and run
//! `<bin> --version`.
//!
//! The argv construction is pure ([`build_smoke_argv`]) so the exact
//! `docker run ...` command is unit-testable without a Docker daemon. The
//! spawn ([`run_smoke`]) is gated behind a daemon-availability probe
//! ([`docker_available`]): when Docker is absent the smoke-test SKIPS with a
//! notice rather than hard-failing the gate (the asset-existence and
//! libc-ceiling checks need neither Docker nor the network and still run).
//!
//! This module owns the only `Command::new("docker")` call site in the
//! verify-release stage. Stage crates are allow-listed for direct subprocess
//! spawn (see `.claude/rules/module-boundaries.md`).

use std::process::Command;

/// The Linux package type a smoke-test targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageType {
    /// Debian/Ubuntu `.deb` (installed via `dpkg -i`).
    Deb,
    /// RPM `.rpm` (installed via `rpm -i`).
    Rpm,
    /// Alpine `.apk` (installed via `apk add --allow-untrusted`).
    Apk,
}

impl PackageType {
    /// Classify a package by its filename extension. Returns `None` for
    /// anything that is not a recognised Linux package.
    pub fn from_filename(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".deb") {
            Some(Self::Deb)
        } else if lower.ends_with(".rpm") {
            Some(Self::Rpm)
        } else if lower.ends_with(".apk") {
            Some(Self::Apk)
        } else {
            None
        }
    }

    /// The in-container install command for this package type, given the
    /// package's basename as mounted at `/pkg/<name>`.
    fn install_cmd(self, pkg_name: &str) -> String {
        // The package name is config-derived and spliced into a `sh -c`
        // string; single-quote it so a name with shell metacharacters cannot
        // break out of the `/pkg/...` token and inject commands.
        let path = sh_single_quote(&format!("/pkg/{pkg_name}"));
        match self {
            // `dpkg -i` then `apt-get -f` to pull any missing deps that the
            // bare `.deb` install left unsatisfied.
            Self::Deb => format!("dpkg -i {path} || (apt-get update && apt-get -y -f install)"),
            Self::Rpm => format!("rpm -i --nodeps {path}"),
            Self::Apk => format!("apk add --allow-untrusted {path}"),
        }
    }
}

/// Single-quote a token for safe interpolation into a `sh -c` string.
///
/// Wraps the value in single quotes and escapes any embedded single quote via
/// the standard `'\''` close-reopen trick, so the value is always treated as
/// one inert literal argument by the shell — never as syntax. The `docker run`
/// argv elements (`-v` mount, image) are already passed as discrete argv and
/// don't need this; only the `sh -c <script>` body, which is a single shell
/// string, does.
fn sh_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A fully-specified smoke-test invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmokeJob {
    /// Container image to run the install + version-check in.
    pub image: String,
    /// Package type (selects the install command).
    pub package_type: PackageType,
    /// Absolute host path to the package file (bind-mounted read-only).
    pub host_pkg_path: String,
    /// The package file's basename (mounted at `/pkg/<name>`).
    pub pkg_name: String,
    /// The installed binary name to version-check (`<bin> --version`).
    pub binary: String,
}

/// Construct the full `docker run ...` argv for a smoke job.
///
/// Shape:
/// ```text
/// docker run --rm \
///   --mount type=bind,source=<host_pkg_path>,destination=/pkg/<pkg_name>,readonly \
///   <image> \
///   sh -c "<install cmd> && <binary> --version"
/// ```
///
/// The bind mount uses `--mount` (comma-separated `key=value` fields) rather
/// than `-v` (`source:dest:opts`): the `-v` form splits its spec on `:`, so a
/// host path or package name containing a colon would silently corrupt the
/// mount. `--mount` does not colon-split, making the mount robust for the full
/// range of artifact path/name characters anodizer produces.
///
/// Network is left enabled so the `.deb` install's `apt-get -f` can pull
/// missing runtime dependencies (the `|| (...)` fixup only runs when the bare
/// `dpkg -i` left deps unsatisfied). The package is bind-mounted read-only so
/// the container cannot mutate the host artifact.
pub fn build_smoke_argv(job: &SmokeJob) -> Vec<String> {
    let mount = format!(
        "type=bind,source={},destination=/pkg/{},readonly",
        job.host_pkg_path, job.pkg_name
    );
    let install = job.package_type.install_cmd(&job.pkg_name);
    let script = format!("{install} && {} --version", sh_single_quote(&job.binary));
    vec![
        "run".to_string(),
        "--rm".to_string(),
        "--mount".to_string(),
        mount,
        job.image.clone(),
        "sh".to_string(),
        "-c".to_string(),
        script,
    ]
}

/// Probe whether a Docker daemon is reachable (`docker version` exits zero).
///
/// `false` when `docker` is missing or the daemon is unreachable — the caller
/// then SKIPS the smoke-test with a notice instead of hard-failing.
pub fn docker_available() -> bool {
    Command::new("docker")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Result of running a single smoke job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmokeOutcome {
    /// The install + `--version` succeeded (container exited zero).
    Passed,
    /// The container exited non-zero; `detail` carries the captured output
    /// tail for diagnosis.
    Failed { detail: String },
}

/// Run a smoke job by spawning `docker run ...`.
///
/// Returns `Ok(SmokeOutcome)` regardless of the container's exit status (a
/// failed install/version-check is a reported defect, not a spawn error);
/// returns `Err` only when `docker` itself could not be spawned.
pub fn run_smoke(job: &SmokeJob) -> anyhow::Result<SmokeOutcome> {
    let argv = build_smoke_argv(job);
    let output = Command::new("docker")
        .args(&argv)
        .output()
        .map_err(|e| anyhow::anyhow!("verify-release: spawning `docker run` failed: {e}"))?;
    if output.status.success() {
        Ok(SmokeOutcome::Passed)
    } else {
        let mut detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if detail.is_empty() {
            detail = String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
        Ok(SmokeOutcome::Failed { detail })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(image: &str, pt: PackageType, bin: &str) -> SmokeJob {
        SmokeJob {
            image: image.to_string(),
            package_type: pt,
            host_pkg_path: "/dist/myapp_1.0_amd64.deb".to_string(),
            pkg_name: "myapp_1.0_amd64.deb".to_string(),
            binary: bin.to_string(),
        }
    }

    #[test]
    fn package_type_from_filename() {
        assert_eq!(
            PackageType::from_filename("a_1.0_amd64.deb"),
            Some(PackageType::Deb)
        );
        assert_eq!(
            PackageType::from_filename("a-1.0.x86_64.RPM"),
            Some(PackageType::Rpm)
        );
        assert_eq!(
            PackageType::from_filename("a-1.0.apk"),
            Some(PackageType::Apk)
        );
        assert_eq!(PackageType::from_filename("a-1.0.tar.gz"), None);
        assert_eq!(PackageType::from_filename("checksums.txt"), None);
    }

    #[test]
    fn deb_argv_has_image_install_and_version_check() {
        let argv = build_smoke_argv(&job("debian:12", PackageType::Deb, "myapp"));
        assert_eq!(argv[0], "run");
        assert!(argv.contains(&"--rm".to_string()));
        // image present as a positional before `sh`.
        let sh_pos = argv.iter().position(|a| a == "sh").unwrap();
        assert_eq!(argv[sh_pos - 1], "debian:12");
        // mount references the host path and the in-container /pkg path via
        // `--mount` (colon-safe), not the colon-splitting `-v` form.
        assert!(
            argv.contains(&"--mount".to_string()),
            "uses --mount: {argv:?}"
        );
        assert!(
            !argv.contains(&"-v".to_string()),
            "no colon-split -v: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a
                == "type=bind,source=/dist/myapp_1.0_amd64.deb,destination=/pkg/myapp_1.0_amd64.deb,readonly"),
            "bind mount argv: {argv:?}"
        );
        let script = argv.last().unwrap();
        assert!(
            script.contains("dpkg -i '/pkg/myapp_1.0_amd64.deb'"),
            "{script}"
        );
        assert!(script.contains("'myapp' --version"), "{script}");
    }

    #[test]
    fn rpm_argv_uses_rpm_install() {
        let argv = build_smoke_argv(&SmokeJob {
            image: "fedora:40".to_string(),
            package_type: PackageType::Rpm,
            host_pkg_path: "/dist/myapp.rpm".to_string(),
            pkg_name: "myapp.rpm".to_string(),
            binary: "myapp".to_string(),
        });
        let script = argv.last().unwrap();
        assert!(
            script.starts_with("rpm -i --nodeps '/pkg/myapp.rpm'"),
            "{script}"
        );
        assert!(script.contains("'myapp' --version"));
    }

    #[test]
    fn apk_argv_uses_apk_add() {
        let argv = build_smoke_argv(&SmokeJob {
            image: "alpine:3.20".to_string(),
            package_type: PackageType::Apk,
            host_pkg_path: "/dist/myapp.apk".to_string(),
            pkg_name: "myapp.apk".to_string(),
            binary: "myapp".to_string(),
        });
        let script = argv.last().unwrap();
        assert!(
            script.starts_with("apk add --allow-untrusted '/pkg/myapp.apk'"),
            "{script}"
        );
        assert!(script.contains("'myapp' --version"));
    }

    #[test]
    fn shell_metacharacters_are_quoted_not_injected() {
        // A binary / package name carrying shell metacharacters must be
        // splice-safe: it lands inside single quotes as one inert literal,
        // never as executable syntax in the `sh -c` body.
        let job = SmokeJob {
            image: "debian:12".to_string(),
            package_type: PackageType::Deb,
            host_pkg_path: "/dist/evil.deb".to_string(),
            pkg_name: "evil; rm -rf /.deb".to_string(),
            binary: "app$(touch pwned)".to_string(),
        };
        let argv = build_smoke_argv(&job);
        let script = argv.last().unwrap();
        // The package name's `;` must be quoted, not a command separator.
        assert!(
            script.contains("dpkg -i '/pkg/evil; rm -rf /.deb'"),
            "pkg name not single-quoted: {script}"
        );
        // The binary's `$(...)` must be quoted, not a command substitution.
        assert!(
            script.contains("'app$(touch pwned)' --version"),
            "binary not single-quoted: {script}"
        );
        // No bare injection token escapes the quoting.
        assert!(
            !script.contains("; rm -rf /.deb'/pkg"),
            "metachar broke out of quoting: {script}"
        );
    }

    #[test]
    fn mount_path_with_colon_is_not_corrupted() {
        // A host path containing a colon would split the legacy `-v src:dst:opt`
        // spec into the wrong fields; `--mount`'s comma-separated key=value
        // syntax keeps the colon inside the `source=` value untouched.
        let argv = build_smoke_argv(&SmokeJob {
            image: "debian:12".to_string(),
            package_type: PackageType::Deb,
            host_pkg_path: "/dist/v1:2/myapp.deb".to_string(),
            pkg_name: "myapp.deb".to_string(),
            binary: "myapp".to_string(),
        });
        assert!(
            argv.iter().any(|a| a
                == "type=bind,source=/dist/v1:2/myapp.deb,destination=/pkg/myapp.deb,readonly"),
            "colon in host path must survive intact in source=: {argv:?}"
        );
        assert!(!argv.contains(&"-v".to_string()));
    }

    #[test]
    fn embedded_single_quote_is_escaped() {
        // The `'\''` close-reopen trick must neutralise an embedded quote.
        assert_eq!(sh_single_quote("a'b"), r"'a'\''b'");
        // A value with no quote is simply wrapped.
        assert_eq!(sh_single_quote("plain"), "'plain'");
    }
}
