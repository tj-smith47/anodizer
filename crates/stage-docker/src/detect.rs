use std::process::Command;
use std::sync::OnceLock;

use anodizer_core::log::StageLogger;

// ---------------------------------------------------------------------------
// is_retriable_error
// ---------------------------------------------------------------------------

/// Determine whether a docker error message indicates a transient failure
/// worth retrying. Matches GoReleaser's narrow `isRetriablePush`
/// (internal/pipe/docker/docker.go:389-418): `io.EOF` plus the exact HTTP
/// status set 500/502/503/504/506/510. Network-level failures (`dial tcp`,
/// `connection refused`, TLS handshake, DNS `no such host`, `REFUSED_STREAM`,
/// `timeout`) fast-fail — waiting through a 10× exponential backoff when the
/// registry is unreachable just delays the inevitable error for the user.
///
/// Used by the legacy (V1) docker build path. V2 uses [`is_retriable_error_v2`].
pub fn is_retriable_error(error_msg: &str) -> bool {
    if error_msg == "EOF" || error_msg.ends_with(": EOF") || error_msg.contains("\nEOF\n") {
        return true;
    }
    let retriable_patterns = [
        "received unexpected HTTP status: 500 Internal Server Error",
        "received unexpected HTTP status: 502 Bad Gateway",
        "received unexpected HTTP status: 503 Service Unavailable",
        "received unexpected HTTP status: 504 Gateway Timeout",
        "received unexpected HTTP status: 506 Variant Also Negotiates",
        "received unexpected HTTP status: 510 Not Extended",
    ];
    retriable_patterns.iter().any(|p| error_msg.contains(p))
}

/// V2-specific retry predicate. Matches GoReleaser's narrow
/// `isRetriableManifestCreate` (`v2/docker.go:544-549`): only retries when
/// the output contains `"manifest verification failed for digest"`. All
/// other errors — network timeouts, build failures, registry 5xx — are
/// considered fatal under V2, because V2 runs `buildx build --push` as a
/// single atomic operation and its own internal retry already covers the
/// lower-level transient cases.
pub fn is_retriable_error_v2(error_msg: &str) -> bool {
    error_msg
        .to_lowercase()
        .contains("manifest verification failed for digest")
}

// ---------------------------------------------------------------------------
// docker_supports_provenance  (cached probe)
// ---------------------------------------------------------------------------

/// Cached result of probing `docker buildx build --help` for `--provenance`.
///
/// GoReleaser probes `docker build --help` output before unconditionally
/// adding `--provenance=false` and `--sbom=false`.  We do the same: run the
/// help command once, cache the result, and only add the flags when the
/// installed Docker version actually recognises them.
static DOCKER_SUPPORTS_PROVENANCE: OnceLock<bool> = OnceLock::new();

pub(crate) fn docker_supports_provenance() -> bool {
    *DOCKER_SUPPORTS_PROVENANCE.get_or_init(|| {
        // Capability probe — no context env injection needed (reads --help output only).
        // Try `docker buildx build --help` first (buildx is the common path).
        // Fall back to `docker build --help` for non-buildx installs.
        let output = Command::new("docker")
            .args(["buildx", "build", "--help"])
            .output()
            .or_else(|_| Command::new("docker").args(["build", "--help"]).output());

        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                stdout.contains("--provenance")
            }
            Err(_) => false, // docker not available — skip the flags
        }
    })
}

// ---------------------------------------------------------------------------
// is_docker_daemon_available
// ---------------------------------------------------------------------------

/// Cached result of probing `docker info` for daemon availability.
///
/// `docker info` can take several seconds when the daemon is down, so we
/// cache the result for the lifetime of the process — consistent with how
/// `DOCKER_SUPPORTS_PROVENANCE` caches its probe result.
static DOCKER_DAEMON_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Check if the Docker daemon is available by running `docker info`.
pub(crate) fn is_docker_daemon_available() -> bool {
    *DOCKER_DAEMON_AVAILABLE.get_or_init(|| {
        Command::new("docker")
            .arg("info")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

// ---------------------------------------------------------------------------
// check_buildx_driver
// ---------------------------------------------------------------------------

/// Outcome of probing `docker buildx version`.
///
/// Modelled as a small enum so [`format_buildx_version_warning`] is pure and
/// table-testable independent of the host's docker install. Production code
/// produces values from [`check_buildx_version`]; tests can construct them
/// directly to exercise the warning surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BuildxVersionProbe {
    /// `docker buildx version` exited 0.
    Available,
    /// `Command::new("docker")` failed to launch (binary missing or unspawnable).
    DockerMissing,
    /// `docker` ran but `buildx version` returned a non-zero status. Stderr
    /// is captured verbatim so the user-facing warning can echo it.
    BuildxMissing { stderr: String },
}

/// Format the user-facing warning for a buildx-version probe outcome.
///
/// Returns `None` when buildx is available (no warning needed). The
/// "DockerMissing" and "BuildxMissing" variants both produce warnings that
/// name buildx explicitly so the user can act, mirroring GoReleaser's
/// `docker buildx version` healthcheck added in commit e09e23a (#6526).
pub(crate) fn format_buildx_version_warning(probe: &BuildxVersionProbe) -> Option<String> {
    match probe {
        BuildxVersionProbe::Available => None,
        BuildxVersionProbe::DockerMissing => Some(
            "docker is not installed or not in PATH; docker_v2 configs require docker \
             with the buildx plugin"
                .to_string(),
        ),
        BuildxVersionProbe::BuildxMissing { stderr } => Some(format!(
            "docker buildx version probe failed; docker_v2 configs require the buildx \
             plugin to be installed. stderr: {}",
            stderr.trim()
        )),
    }
}

/// Run the buildx-version probe and emit a warning on the supplied logger if
/// the probe reports an actionable failure. The probe is supplied as a
/// closure so tests can inject deterministic outcomes; production callers
/// pass [`probe_buildx_version`] (which shells out to `docker buildx
/// version`).
pub(crate) fn run_buildx_version_check<F>(log: &StageLogger, probe: F)
where
    F: FnOnce() -> BuildxVersionProbe,
{
    if let Some(msg) = format_buildx_version_warning(&probe()) {
        log.warn(&msg);
    }
}

/// Probe `docker buildx version` and classify the outcome.
///
/// Used by [`check_buildx_version`] to feed [`run_buildx_version_check`].
/// Lives next to the other `Command::new` probes in this module so the
/// `module-boundaries.md` allow-list stays accurate.
pub(crate) fn probe_buildx_version() -> BuildxVersionProbe {
    // Capability probe: no context env injection needed (reads version only).
    let output = Command::new("docker").args(["buildx", "version"]).output();
    match output {
        Err(_) => BuildxVersionProbe::DockerMissing,
        Ok(o) if o.status.success() => BuildxVersionProbe::Available,
        Ok(o) => BuildxVersionProbe::BuildxMissing {
            stderr: String::from_utf8_lossy(&o.stderr).to_string(),
        },
    }
}

/// Convenience wrapper: probe `docker buildx version` and warn on the supplied
/// logger if buildx is unavailable. Complements [`check_buildx_driver`]: this
/// confirms the plugin is reachable, while `check_buildx_driver` validates
/// the active driver. Both are lenient (warn-only) because docker setups vary
/// and downstream `buildx build` will surface a hard error if it actually
/// cannot run.
pub(crate) fn check_buildx_version(log: &StageLogger) {
    run_buildx_version_check(log, probe_buildx_version);
}

/// Check the current buildx driver and warn if it is not one of the standard
/// types ("docker-container" or "docker").
///
/// GoReleaser v2 validates the driver via `docker buildx inspect` and errors
/// on invalid drivers. We warn rather than error to be lenient, but the
/// check ensures users know their setup may not work for multi-platform builds.
pub(crate) fn check_buildx_driver(log: &StageLogger) {
    // Capability probe — no context env injection needed (reads driver info only).
    let output = Command::new("docker").args(["buildx", "inspect"]).output();
    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            // Parse the Driver line from `docker buildx inspect` output.
            // Example: "Driver:           docker-container"
            for line in stdout.lines() {
                if let Some(driver) = line.strip_prefix("Driver:") {
                    let driver = driver.trim();
                    if driver != "docker-container" && driver != "docker" {
                        log.warn(&format!(
                            "buildx driver '{}' is not 'docker-container' or 'docker'; \
                             multi-platform builds may not work correctly",
                            driver
                        ));
                    }
                    return;
                }
            }
            // Driver line not found in output — warn about unknown driver
            log.warn("could not determine buildx driver from 'docker buildx inspect' output");
        }
        Err(_) => {
            // docker buildx not available — skip the check
        }
    }
}
