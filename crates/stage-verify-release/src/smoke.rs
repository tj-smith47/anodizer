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
    /// package's absolute path inside the container.
    fn install_cmd(self, container_pkg_path: &str) -> String {
        // The path is config-derived and spliced into a `sh -c` string;
        // single-quote it so a name with shell metacharacters cannot break out
        // of the token and inject commands.
        let path = sh_single_quote(container_pkg_path);
        match self {
            // `dpkg -i` then `apt-get -f` to pull any missing deps that the
            // bare `.deb` install left unsatisfied.
            Self::Deb => format!("dpkg -i {path} || (apt-get update && apt-get -y -f install)"),
            Self::Rpm => format!("rpm -i --nodeps {path}"),
            Self::Apk => format!("apk add --allow-untrusted {path}"),
        }
    }
}

/// The `sh -c` body run inside the smoke container: install the package found at
/// `container_pkg_path`, then version-check the installed binary. Identical
/// across the bind-mount and copy strategies — only how the package reaches that
/// path differs.
fn smoke_script(job: &SmokeJob, container_pkg_path: &str) -> String {
    let install = job.package_type.install_cmd(container_pkg_path);
    format!("{install} && {} --version", sh_single_quote(&job.binary))
}

/// Extract a diagnostic detail string from a finished process: prefer stderr,
/// fall back to stdout.
fn output_detail(out: &std::process::Output) -> String {
    let mut detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if detail.is_empty() {
        detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
    }
    detail
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
    let container_path = format!("/pkg/{}", job.pkg_name);
    let mount = format!(
        "type=bind,source={},destination={},readonly",
        job.host_pkg_path, container_path
    );
    let script = smoke_script(job, &container_path);
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

/// The container path a copy-strategy smoke job installs from. The package is
/// `docker cp`-ed to the container root (which always exists) rather than a
/// `/pkg/` subdir that the base image may lack.
fn copy_container_path(job: &SmokeJob) -> String {
    format!("/{}", job.pkg_name)
}

/// `docker create` argv for the copy strategy: define the install + version
/// container without starting it, so the package can be copied in first. No
/// `--mount` — the package arrives via [`build_copy_cp_argv`].
fn build_copy_create_argv(job: &SmokeJob) -> Vec<String> {
    let script = smoke_script(job, &copy_container_path(job));
    vec![
        "create".to_string(),
        job.image.clone(),
        "sh".to_string(),
        "-c".to_string(),
        script,
    ]
}

/// `docker cp` argv: stream the host package into the created container at its
/// root. `docker cp` transfers over the daemon socket (path-agnostic, like a
/// buildx context), so it works even when the daemon's filesystem is separate
/// from ours (dind without a shared work dir).
fn build_copy_cp_argv(job: &SmokeJob, container_id: &str) -> Vec<String> {
    vec![
        "cp".to_string(),
        job.host_pkg_path.clone(),
        format!("{container_id}:/{}", job.pkg_name),
    ]
}

/// `docker start -a` argv: run the created container and attach so its exit
/// status and output are captured.
fn build_copy_start_argv(container_id: &str) -> Vec<String> {
    vec![
        "start".to_string(),
        "-a".to_string(),
        container_id.to_string(),
    ]
}

/// `docker rm -f` argv: best-effort teardown of the copy-strategy container
/// (the create path omits `--rm` so the container survives a failed cp/start
/// for cleanup here).
fn build_copy_rm_argv(container_id: &str) -> Vec<String> {
    vec!["rm".to_string(), "-f".to_string(), container_id.to_string()]
}

/// Probe whether a Docker daemon is reachable (`docker version` exits zero).
///
/// `false` when `docker` is missing or the daemon is unreachable — the caller
/// then SKIPS the smoke-test with a notice instead of hard-failing.
pub fn docker_available() -> bool {
    Command::new("docker")
        .arg("version")
        .current_dir(anodizer_core::path_util::probe_dir())
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

/// How the package reaches the smoke container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MountStrategy {
    /// `docker run --mount type=bind` — the host package path is visible to the
    /// daemon (local daemon, or dind with the work dir shared in at the same
    /// path). Fast and proven; the preferred path.
    BindMount,
    /// `docker create` + `docker cp` + `docker start` — the package is streamed
    /// over the daemon socket. Used when a bind mount of a host path is NOT
    /// visible inside the container (separate-filesystem dind without a shared
    /// work dir), where the bind path would resolve empty.
    Copy,
}

impl MountStrategy {
    /// A short, log-friendly label for the resolved strategy.
    fn label(self) -> &'static str {
        match self {
            Self::BindMount => "bind-mount",
            Self::Copy => "docker cp (separate-filesystem daemon)",
        }
    }
}

/// Resolve (once, cached) which [`MountStrategy`] this daemon supports.
///
/// `probe_image` is run with `cat` for the sentinel read-back; the caller passes
/// a smoke image it is about to use anyway, so the probe adds no extra image
/// pull. The result is daemon-wide (filesystem visibility, not image-specific),
/// so the first call's image determines the cached value for the whole process.
fn mount_strategy(probe_image: &str) -> MountStrategy {
    static CACHE: std::sync::OnceLock<MountStrategy> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| probe_mount_strategy(probe_image))
}

/// Decide whether `--mount type=bind` of a host file surfaces inside the
/// container. Under a separate-filesystem dind sidecar (daemon in another
/// container without the runner's workspace shared in), a bind source resolves
/// against the daemon's filesystem and the file is absent — so the package must
/// be `docker cp`-ed in instead. Probe by binding a sentinel file and reading it
/// back: matching content ⇒ bind mounts are usable. Any failure (write denied,
/// daemon/pull error, content mismatch) falls back to the portable copy path.
fn probe_mount_strategy(probe_image: &str) -> MountStrategy {
    let dir = anodizer_core::path_util::probe_dir();
    let token = "anodizer-smoke-probe-ok";
    let sentinel = dir.join(".anodizer-smoke-probe");
    if std::fs::write(&sentinel, token).is_err() {
        return MountStrategy::Copy;
    }
    let abs = std::fs::canonicalize(&sentinel).unwrap_or_else(|_| sentinel.clone());
    let argv = [
        "run".to_string(),
        "--rm".to_string(),
        "--mount".to_string(),
        format!(
            "type=bind,source={},destination=/probe,readonly",
            abs.display()
        ),
        probe_image.to_string(),
        "cat".to_string(),
        "/probe".to_string(),
    ];
    let out = Command::new("docker")
        .args(&argv)
        .current_dir(&dir)
        .output();
    let _ = std::fs::remove_file(&sentinel);
    match out {
        Ok(o) if o.status.success() && String::from_utf8_lossy(&o.stdout).contains(token) => {
            MountStrategy::BindMount
        }
        _ => MountStrategy::Copy,
    }
}

/// Resolve the install-smoke strategy (probing once with `probe_image`) and
/// return its log-friendly label, so the caller can emit a one-time breadcrumb
/// explaining why subsequent smoke tests take the bind-mount or copy path.
pub fn strategy_label(probe_image: &str) -> &'static str {
    mount_strategy(probe_image).label()
}

/// Run a smoke job, choosing the bind-mount or copy strategy based on a
/// one-time probe of the Docker daemon's filesystem visibility.
///
/// Returns `Ok(SmokeOutcome)` regardless of the container's exit status (a
/// failed install/version-check is a reported defect, not a spawn error);
/// returns `Err` only when `docker` itself could not be spawned.
pub fn run_smoke(job: &SmokeJob) -> anyhow::Result<SmokeOutcome> {
    match mount_strategy(&job.image) {
        MountStrategy::BindMount => run_smoke_bind(job),
        MountStrategy::Copy => run_smoke_copy(job),
    }
}

/// Bind-mount strategy: a single `docker run --mount` invocation.
fn run_smoke_bind(job: &SmokeJob) -> anyhow::Result<SmokeOutcome> {
    let argv = build_smoke_argv(job);
    let output = Command::new("docker")
        .args(&argv)
        .output()
        .map_err(|e| anyhow::anyhow!("verify-release: spawning `docker run` failed: {e}"))?;
    if output.status.success() {
        Ok(SmokeOutcome::Passed)
    } else {
        Ok(SmokeOutcome::Failed {
            detail: output_detail(&output),
        })
    }
}

/// Copy strategy: `docker create` → `docker cp` the package in → `docker start`
/// → best-effort `docker rm`. The container is always torn down, even when an
/// intermediate step fails.
fn run_smoke_copy(job: &SmokeJob) -> anyhow::Result<SmokeOutcome> {
    let create = Command::new("docker")
        .args(build_copy_create_argv(job))
        .output()
        .map_err(|e| anyhow::anyhow!("verify-release: spawning `docker create` failed: {e}"))?;
    if !create.status.success() {
        return Ok(SmokeOutcome::Failed {
            detail: output_detail(&create),
        });
    }
    let container_id = String::from_utf8_lossy(&create.stdout).trim().to_string();
    if container_id.is_empty() {
        return Ok(SmokeOutcome::Failed {
            detail: "verify-release: `docker create` returned no container id".to_string(),
        });
    }

    let outcome = run_smoke_copy_inner(job, &container_id);

    // Best-effort teardown regardless of how the run above resolved.
    let _ = Command::new("docker")
        .args(build_copy_rm_argv(&container_id))
        .output();

    outcome
}

/// The cp + start steps of the copy strategy, factored out so the caller can
/// always run teardown afterward.
fn run_smoke_copy_inner(job: &SmokeJob, container_id: &str) -> anyhow::Result<SmokeOutcome> {
    let cp = Command::new("docker")
        .args(build_copy_cp_argv(job, container_id))
        .output()
        .map_err(|e| anyhow::anyhow!("verify-release: spawning `docker cp` failed: {e}"))?;
    if !cp.status.success() {
        return Ok(SmokeOutcome::Failed {
            detail: output_detail(&cp),
        });
    }
    let start = Command::new("docker")
        .args(build_copy_start_argv(container_id))
        .output()
        .map_err(|e| anyhow::anyhow!("verify-release: spawning `docker start` failed: {e}"))?;
    if start.status.success() {
        Ok(SmokeOutcome::Passed)
    } else {
        Ok(SmokeOutcome::Failed {
            detail: output_detail(&start),
        })
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

    #[test]
    fn copy_create_argv_has_no_mount_and_installs_from_root() {
        // The copy strategy must NOT bind-mount (that is the broken-under-dind
        // path); it defines the container and installs from the root path the
        // package is later `docker cp`-ed to.
        let argv = build_copy_create_argv(&job("debian:12", PackageType::Deb, "myapp"));
        assert_eq!(argv[0], "create");
        assert!(
            !argv.contains(&"--mount".to_string()),
            "no bind mount: {argv:?}"
        );
        assert!(!argv.contains(&"-v".to_string()), "no -v mount: {argv:?}");
        let sh_pos = argv.iter().position(|a| a == "sh").unwrap();
        assert_eq!(argv[sh_pos - 1], "debian:12");
        let script = argv.last().unwrap();
        assert!(
            script.contains("dpkg -i '/myapp_1.0_amd64.deb'"),
            "installs from container root: {script}"
        );
        assert!(script.contains("'myapp' --version"), "{script}");
    }

    #[test]
    fn copy_cp_argv_targets_container_root() {
        let argv = build_copy_cp_argv(&job("debian:12", PackageType::Deb, "myapp"), "abc123");
        assert_eq!(
            argv,
            vec![
                "cp".to_string(),
                "/dist/myapp_1.0_amd64.deb".to_string(),
                "abc123:/myapp_1.0_amd64.deb".to_string(),
            ]
        );
    }

    #[test]
    fn copy_start_and_rm_argv() {
        assert_eq!(
            build_copy_start_argv("abc123"),
            vec!["start".to_string(), "-a".to_string(), "abc123".to_string()]
        );
        assert_eq!(
            build_copy_rm_argv("abc123"),
            vec!["rm".to_string(), "-f".to_string(), "abc123".to_string()]
        );
    }

    #[test]
    fn copy_rpm_and_apk_install_from_root() {
        let rpm = build_copy_create_argv(&SmokeJob {
            image: "fedora:40".to_string(),
            package_type: PackageType::Rpm,
            host_pkg_path: "/dist/myapp.rpm".to_string(),
            pkg_name: "myapp.rpm".to_string(),
            binary: "myapp".to_string(),
        });
        assert!(
            rpm.last()
                .unwrap()
                .starts_with("rpm -i --nodeps '/myapp.rpm'"),
            "{:?}",
            rpm.last()
        );

        let apk = build_copy_create_argv(&SmokeJob {
            image: "alpine:3.20".to_string(),
            package_type: PackageType::Apk,
            host_pkg_path: "/dist/myapp.apk".to_string(),
            pkg_name: "myapp.apk".to_string(),
            binary: "myapp".to_string(),
        });
        assert!(
            apk.last()
                .unwrap()
                .starts_with("apk add --allow-untrusted '/myapp.apk'"),
            "{:?}",
            apk.last()
        );
    }

    #[test]
    fn copy_script_quotes_metacharacters() {
        // The copy path shares smoke_script with the bind path, so the same
        // single-quote splice-safety must hold for the container-root path.
        let argv = build_copy_create_argv(&SmokeJob {
            image: "debian:12".to_string(),
            package_type: PackageType::Deb,
            host_pkg_path: "/dist/evil.deb".to_string(),
            pkg_name: "evil; rm -rf /.deb".to_string(),
            binary: "app$(touch pwned)".to_string(),
        });
        let script = argv.last().unwrap();
        assert!(
            script.contains("dpkg -i '/evil; rm -rf /.deb'"),
            "pkg name not single-quoted: {script}"
        );
        assert!(
            script.contains("'app$(touch pwned)' --version"),
            "binary not single-quoted: {script}"
        );
    }
}
