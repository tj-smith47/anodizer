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

/// Sentinel printed to stdout AFTER a successful install and BEFORE the
/// version-check runs. Its presence in the captured stdout tells the failure
/// classifier which step failed: absent ⇒ install failed; present ⇒ install
/// succeeded and the version-check is the failing step.
///
/// The classifier matches the marker only when it occupies its own line (the
/// position our `printf '%s\n'` writes it), so a package that happened to echo
/// the token mid-line during a failing install cannot forge an install-success
/// signal. The token is also unique and emitted by anodize, not by any known
/// package manager — but the own-line anchor is the actual guarantee.
const SMOKE_STEP_MARKER: &str = "__ANODIZER_SMOKE_INSTALLED__";

/// `true` when `stdout` contains [`SMOKE_STEP_MARKER`] on its own line — the
/// exact shape `printf '%s\n' <marker>` produces. Matching a whole line (rather
/// than a bare substring) means stray package output that merely embeds the
/// token cannot be mistaken for the install-success signal.
fn marker_present(stdout: &str) -> bool {
    stdout.lines().any(|line| line == SMOKE_STEP_MARKER)
}

/// `true` when `stderr` looks like a Docker-daemon / runtime failure to START
/// the container (image pull error, exec error, OCI runtime error) rather than
/// a failure of the install or version-check command running INSIDE it. The
/// container never reaches the install step in this case, so attributing the
/// failure to "install" would mislead. Detected by the `docker:`-prefixed
/// runtime error line and the daemon's canonical error envelope.
fn is_container_start_failure(stderr: &str) -> bool {
    stderr
        .lines()
        .any(|line| line.starts_with("docker:") || line.contains("Error response from daemon"))
}

/// The `sh -c` body run inside the smoke container: install the package found at
/// `container_pkg_path`, then version-check the installed binary. Identical
/// across the bind-mount and copy strategies — only how the package reaches that
/// path differs.
///
/// A [`SMOKE_STEP_MARKER`] is printed between the two steps so a failure can be
/// attributed to the install OR the version-check: chaining them with a bare
/// `&&` merged their output and hid which step failed (the v0.9.0 apk smoke
/// reported the install's "OK: ... 17 packages" success while the real failure
/// was `--version` exiting 127 on a glibc binary under musl).
fn smoke_script(job: &SmokeJob, container_pkg_path: &str) -> String {
    let install = job.package_type.install_cmd(container_pkg_path);
    // `printf` (not `echo`) for portable, flag-free marker emission; only runs
    // when install succeeded (`&&`), so the marker's presence is a reliable
    // install-success signal.
    format!(
        "{install} && printf '%s\\n' {marker} && {bin} --version",
        marker = sh_single_quote(SMOKE_STEP_MARKER),
        bin = sh_single_quote(&job.binary)
    )
}

/// Extract a diagnostic detail string from a finished process: the exit status
/// plus stderr and a tail of stdout. Used for the non-install/version steps
/// (emulation probe, `docker create`/`cp`) where there is no install-vs-check
/// boundary to attribute.
///
/// Always surfaces the exit code — for an exec failure (glibc binary on musl)
/// the bare `127` IS the smoking gun, and it was absent from the v0.9.0 log.
/// stderr carries the real error; a stdout tail is appended so a step that
/// reports its failure on stdout (or succeeds noisily before failing) is not
/// masked.
fn output_detail(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    compose_detail(&out.status, stderr.trim(), stdout.trim())
}

/// Assemble a `(exit <code>) stderr=… stdout=…` detail string, omitting the
/// empty streams. Shared by [`output_detail`] and [`smoke_failure_detail`] so
/// the exit code is never dropped.
fn compose_detail(status: &std::process::ExitStatus, stderr: &str, stdout: &str) -> String {
    let mut parts = vec![exit_label(status)];
    if !stderr.is_empty() {
        parts.push(format!("stderr: {}", tail(stderr)));
    }
    if !stdout.is_empty() {
        parts.push(format!("stdout: {}", tail(stdout)));
    }
    parts.join(" — ")
}

/// Human label for a process exit: `exit <code>` or `signal <n>` (Unix), so
/// `exit 127` (exec failure) is always visible.
fn exit_label(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit {code}"),
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    return format!("killed by signal {sig}");
                }
            }
            "terminated without an exit code".to_string()
        }
    }
}

/// Last 2 KiB of a stream, prefixed with an elision marker when truncated, so a
/// long install log does not bury the relevant tail (the failing step's output
/// is at the end).
fn tail(s: &str) -> String {
    const MAX: usize = 2048;
    if s.len() <= MAX {
        return s.to_string();
    }
    // Truncate on a char boundary at or after the MAX-byte cut point.
    let mut cut = s.len() - MAX;
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    format!("…(truncated) {}", &s[cut..])
}

/// Build the failure detail for the install+version container exec, attributing
/// the failure to the right step:
///
/// 1. A Docker-daemon/runtime failure to START the container (image pull, exec,
///    OCI runtime error) is labeled "container failed to start" — the container
///    never ran the install, so blaming install would mislead.
/// 2. Marker present ([`SMOKE_STEP_MARKER`] on its own line) ⇒ install
///    succeeded and the version-check is the failing step; only the post-marker
///    stdout is reported so the install's success banner does not mask the real
///    error (the v0.9.0 apk case: install printed "OK: … 17 packages", then
///    `--version` exited 127 — attributed to the version-check, banner dropped).
/// 3. Marker absent ⇒ the install step itself failed; its output is reported.
fn smoke_failure_detail(binary: &str, out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr_trim = stderr.trim();

    if is_container_start_failure(stderr_trim) {
        return format!(
            "container failed to start: {}",
            compose_detail(&out.status, stderr_trim, stdout.trim())
        );
    }

    match stdout.split_once(SMOKE_STEP_MARKER) {
        // Marker present (own-line) ⇒ install succeeded; version-check failed.
        // Report only what the version-check produced on stdout (after the
        // marker line).
        Some((_install_out, version_out)) if marker_present(&stdout) => format!(
            "version-check (`{binary} --version`) failed: {}",
            compose_detail(&out.status, stderr_trim, version_out.trim())
        ),
        // Marker absent (or only mid-line, hence forged) ⇒ install never
        // completed; report the install output.
        _ => format!(
            "install step failed: {}",
            compose_detail(&out.status, stderr_trim, stdout.trim())
        ),
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
    /// Docker platform spec (`linux/arm64`, …) derived from the package's
    /// build target. `Some` pins every container run to the package's
    /// architecture; `None` (host build with no triple) runs on the daemon
    /// default.
    pub platform: Option<String>,
}

/// Map a Rust target triple to the Docker platform spec (`linux/<arch>`)
/// its package must be installed under.
///
/// Returns `None` for non-Linux triples (no Linux package to smoke-test) and
/// for architectures with no known Docker platform name — the caller then
/// omits `--platform`, preserving the daemon-default behavior.
///
/// Pinning the platform matters even for native packages: `docker run` with a
/// bare tag reuses whatever variant of that tag was pulled last, so an arm64
/// smoke run would re-tag e.g. `alpine:latest` to the arm64 variant and a
/// subsequent unpinned amd64 run would inherit the wrong-arch image.
pub fn docker_platform(target_triple: &str) -> Option<String> {
    if !anodizer_core::target::is_linux(target_triple) {
        return None;
    }
    let (_, arch) = anodizer_core::target::map_target(target_triple);
    let docker_arch = match arch.as_str() {
        "amd64" => "amd64",
        "arm64" => "arm64",
        "386" => "386",
        "armv7" => "arm/v7",
        "armv6" => "arm/v6",
        "s390x" => "s390x",
        "ppc64le" => "ppc64le",
        "riscv64" => "riscv64",
        "loong64" => "loong64",
        _ => return None,
    };
    Some(format!("linux/{docker_arch}"))
}

/// The host's Docker platform spec (`linux/<arch>`), or `None` when the host
/// CPU has no known Docker platform name.
fn host_docker_platform() -> Option<String> {
    host_docker_platform_for(std::env::consts::ARCH)
}

/// Pure mapping seam for [`host_docker_platform`]: Rust `target_arch` name →
/// Docker platform spec.
fn host_docker_platform_for(rust_arch: &str) -> Option<String> {
    let docker_arch = match rust_arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "s390x" => "s390x",
        "powerpc64" => "ppc64le",
        "riscv64" => "riscv64",
        "loongarch64" => "loong64",
        _ => return None,
    };
    Some(format!("linux/{docker_arch}"))
}

/// Whether `platform` matches the host's native Docker platform. `false`
/// (forcing an emulation probe) when the host platform is unknown — the
/// probe, not an assumption, then decides whether the job can run.
fn platform_is_native(platform: &str) -> bool {
    host_docker_platform().is_some_and(|host| host == platform)
}

/// The platform a smoke job pins its container to: the package's build
/// target when it maps to a Docker platform, otherwise the HOST platform.
/// The host fallback matters because a prior cross-arch pull re-tags the
/// shared image tag (e.g. `alpine:latest`) to the foreign variant — an
/// unpinned job would inherit that poisoned tag. `None` only when neither
/// the target nor the host CPU has a known Docker platform name.
pub(crate) fn job_platform(target_triple: Option<&str>) -> Option<String> {
    target_triple
        .and_then(docker_platform)
        .or_else(host_docker_platform)
}

/// `docker run` argv probing whether `platform` containers can execute on
/// this daemon: runs the image's `true` binary under the requested platform.
fn build_emulation_probe_argv(platform: &str, image: &str) -> Vec<String> {
    vec![
        "run".to_string(),
        "--rm".to_string(),
        "--platform".to_string(),
        platform.to_string(),
        image.to_string(),
        "true".to_string(),
    ]
}

/// Probe whether the daemon can execute `platform` containers of `image`:
/// `None` when the probe run succeeds, `Some(probe output)` when it fails.
///
/// Cached per (platform, image): one image lacking a variant for an
/// architecture (a pull failure, not missing emulation) must not poison the
/// verdict for other images that do publish that variant. A failed probe
/// means every smoke job for that (platform, image) must FAIL loudly rather
/// than report a misleading in-container arch error or silently drop
/// coverage.
fn emulation_probe_failure(platform: &str, image: &str) -> Option<String> {
    type ProbeCache = std::collections::HashMap<(String, String), Option<String>>;
    static CACHE: std::sync::OnceLock<std::sync::Mutex<ProbeCache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = (platform.to_string(), image.to_string());
    if let Some(known) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return known.clone();
    }
    let failure = match Command::new("docker")
        .args(build_emulation_probe_argv(platform, image))
        .current_dir(anodizer_core::path_util::probe_dir())
        .output()
    {
        Ok(out) if out.status.success() => None,
        Ok(out) => Some(output_detail(&out)),
        Err(e) => Some(format!(
            "spawning the `docker run` platform probe failed: {e}"
        )),
    };
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, failure.clone());
    failure
}

/// The loud, actionable failure detail for a smoke job whose platform probe
/// failed. An `exec format error` in the probe output is the missing
/// qemu/binfmt signature and gets the emulation remediation; any other
/// failure (image pull, daemon error) is reported as a probe failure so the
/// operator isn't sent chasing binfmt for a network problem. The raw probe
/// output is always appended.
fn emulation_unavailable_detail(platform: &str, probe_detail: &str) -> String {
    let host = host_docker_platform().unwrap_or_else(|| "unknown".to_string());
    let cause = if probe_detail.contains("exec format error") {
        format!(
            "cross-arch emulation (qemu/binfmt) is unavailable. Install it (e.g. \
             `docker run --privileged --rm tonistiigi/binfmt --install all`) or run \
             install_smoke on a {platform} runner"
        )
    } else {
        format!("the {platform} platform probe failed (image pull or daemon error)")
    };
    format!(
        "cannot run {platform} containers on this {host} host: {cause}. \
         The package was NOT smoke-tested. Probe output: {probe_detail}"
    )
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
    let mut argv = vec!["run".to_string(), "--rm".to_string()];
    push_platform_flag(&mut argv, job);
    argv.extend([
        "--mount".to_string(),
        mount,
        job.image.clone(),
        "sh".to_string(),
        "-c".to_string(),
        script,
    ]);
    argv
}

/// Append `--platform <spec>` when the job carries a platform, pinning the
/// container (and any image pull) to the package's architecture.
fn push_platform_flag(argv: &mut Vec<String>, job: &SmokeJob) {
    if let Some(platform) = &job.platform {
        argv.push("--platform".to_string());
        argv.push(platform.clone());
    }
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
    let mut argv = vec!["create".to_string()];
    push_platform_flag(&mut argv, job);
    argv.extend([
        job.image.clone(),
        "sh".to_string(),
        "-c".to_string(),
        script,
    ]);
    argv
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
    let mut argv = vec!["run".to_string(), "--rm".to_string()];
    // Pin the probe to the host platform: a prior cross-arch pull may have
    // re-tagged the shared image tag to a foreign variant, and an unpinned
    // probe would then run (and fail on) the wrong-arch image.
    if let Some(host) = host_docker_platform() {
        argv.push("--platform".to_string());
        argv.push(host);
    }
    argv.extend([
        "--mount".to_string(),
        format!(
            "type=bind,source={},destination=/probe,readonly",
            abs.display()
        ),
        probe_image.to_string(),
        "cat".to_string(),
        "/probe".to_string(),
    ]);
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
    if let Some(platform) = &job.platform
        && !platform_is_native(platform)
        && let Some(probe_detail) = emulation_probe_failure(platform, &job.image)
    {
        return Ok(SmokeOutcome::Failed {
            detail: emulation_unavailable_detail(platform, &probe_detail),
        });
    }
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
            detail: smoke_failure_detail(&job.binary, &output),
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
        // `docker start -a` runs the install+version script, so attribute the
        // failure to the right step exactly as the bind-mount path does.
        Ok(SmokeOutcome::Failed {
            detail: smoke_failure_detail(&job.binary, &start),
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
            platform: None,
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
            platform: None,
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
            platform: None,
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
            platform: None,
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
            platform: None,
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
            platform: None,
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
            platform: None,
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
    fn docker_platform_maps_linux_triples() {
        for (triple, want) in [
            ("x86_64-unknown-linux-gnu", "linux/amd64"),
            ("x86_64-unknown-linux-musl", "linux/amd64"),
            ("aarch64-unknown-linux-gnu", "linux/arm64"),
            ("aarch64-unknown-linux-musl", "linux/arm64"),
            ("i686-unknown-linux-gnu", "linux/386"),
            ("armv7-unknown-linux-gnueabihf", "linux/arm/v7"),
            ("armv6l-unknown-linux-gnueabihf", "linux/arm/v6"),
            ("s390x-unknown-linux-gnu", "linux/s390x"),
            ("powerpc64le-unknown-linux-gnu", "linux/ppc64le"),
            ("riscv64gc-unknown-linux-gnu", "linux/riscv64"),
            ("loongarch64-unknown-linux-gnu", "linux/loong64"),
        ] {
            assert_eq!(
                docker_platform(triple).as_deref(),
                Some(want),
                "triple {triple}"
            );
        }
    }

    #[test]
    fn docker_platform_rejects_non_linux_and_unknown() {
        // No Linux package exists for these — no platform to pin.
        assert_eq!(docker_platform("aarch64-apple-darwin"), None);
        assert_eq!(docker_platform("x86_64-pc-windows-msvc"), None);
        assert_eq!(docker_platform("aarch64-linux-android"), None);
        // Linux but no Docker platform name — omit the flag, don't guess.
        assert_eq!(docker_platform("sparc64-unknown-linux-gnu"), None);
    }

    #[test]
    fn host_docker_platform_mapping() {
        assert_eq!(
            host_docker_platform_for("x86_64").as_deref(),
            Some("linux/amd64")
        );
        assert_eq!(
            host_docker_platform_for("aarch64").as_deref(),
            Some("linux/arm64")
        );
        assert_eq!(host_docker_platform_for("sparc64"), None);
    }

    #[test]
    fn run_argv_pins_platform_when_set() {
        let mut j = job("alpine:3.20", PackageType::Apk, "myapp");
        j.platform = Some("linux/arm64".to_string());
        let argv = build_smoke_argv(&j);
        let pos = argv.iter().position(|a| a == "--platform").unwrap();
        assert_eq!(argv[pos + 1], "linux/arm64");
        // The flag must precede the image (docker run options come first).
        let img_pos = argv.iter().position(|a| a == "alpine:3.20").unwrap();
        assert!(pos < img_pos, "--platform after image: {argv:?}");
    }

    #[test]
    fn run_argv_omits_platform_when_unset() {
        let argv = build_smoke_argv(&job("alpine:3.20", PackageType::Apk, "myapp"));
        assert!(
            !argv.contains(&"--platform".to_string()),
            "no platform flag for host builds: {argv:?}"
        );
    }

    #[test]
    fn copy_create_argv_pins_platform_when_set() {
        let mut j = job("debian:12", PackageType::Deb, "myapp");
        j.platform = Some("linux/arm64".to_string());
        let argv = build_copy_create_argv(&j);
        let pos = argv.iter().position(|a| a == "--platform").unwrap();
        assert_eq!(argv[pos + 1], "linux/arm64");
        let img_pos = argv.iter().position(|a| a == "debian:12").unwrap();
        assert!(pos < img_pos, "--platform after image: {argv:?}");

        let bare = build_copy_create_argv(&job("debian:12", PackageType::Deb, "myapp"));
        assert!(!bare.contains(&"--platform".to_string()));
    }

    #[test]
    fn emulation_probe_argv_runs_true_under_platform() {
        assert_eq!(
            build_emulation_probe_argv("linux/arm64", "alpine:latest"),
            vec![
                "run".to_string(),
                "--rm".to_string(),
                "--platform".to_string(),
                "linux/arm64".to_string(),
                "alpine:latest".to_string(),
                "true".to_string(),
            ]
        );
    }

    #[test]
    fn emulation_unavailable_detail_is_actionable() {
        // The binfmt signature gets the emulation remediation.
        let detail =
            emulation_unavailable_detail("linux/arm64", "exec /bin/true: exec format error");
        assert!(detail.contains("linux/arm64"), "{detail}");
        assert!(detail.contains("qemu/binfmt"), "{detail}");
        assert!(detail.contains("tonistiigi/binfmt"), "{detail}");
        assert!(detail.contains("NOT smoke-tested"), "{detail}");
        assert!(
            detail.contains("exec /bin/true: exec format error"),
            "raw probe output appended: {detail}"
        );
    }

    #[test]
    fn emulation_detail_distinguishes_pull_failures_from_missing_binfmt() {
        // A pull/daemon error must NOT send the operator chasing binfmt.
        let detail = emulation_unavailable_detail(
            "linux/arm64",
            "manifest for myimage:latest not found: manifest unknown",
        );
        assert!(
            !detail.contains("qemu/binfmt"),
            "no binfmt remediation for a pull failure: {detail}"
        );
        assert!(
            detail.contains("image pull or daemon error"),
            "names the real failure class: {detail}"
        );
        assert!(
            detail.contains("manifest unknown"),
            "raw probe output appended: {detail}"
        );
        assert!(detail.contains("NOT smoke-tested"), "{detail}");
    }

    #[test]
    fn job_platform_prefers_target_then_host() {
        // A mapped target wins over the host platform.
        assert_eq!(
            job_platform(Some("aarch64-unknown-linux-gnu")).as_deref(),
            Some("linux/arm64")
        );
        // No target (host build) → the host platform, so the job never runs
        // a foreign variant left behind on a shared tag by a cross-arch pull.
        assert_eq!(job_platform(None), host_docker_platform());
        // Unmappable target → host fallback too.
        assert_eq!(
            job_platform(Some("sparc64-unknown-linux-gnu")),
            host_docker_platform()
        );
    }

    /// Build a fake finished `Output` with a given exit code and streams, so
    /// the failure-attribution logic can be tested without a Docker daemon.
    #[cfg(unix)]
    fn fake_output(code: i32, stdout: &str, stderr: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            // `from_raw` takes a wait(2) status; the exit code lives in bits
            // 8-15, so shift left by 8.
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn smoke_script_emits_marker_between_install_and_version() {
        let s = smoke_script(&job("alpine:3.20", PackageType::Apk, "myapp"), "/pkg/x.apk");
        // Install, then marker, then version-check — in order.
        let install_pos = s.find("apk add").expect("install present");
        let marker_pos = s.find(SMOKE_STEP_MARKER).expect("marker present");
        let version_pos = s.find("'myapp' --version").expect("version present");
        assert!(
            install_pos < marker_pos && marker_pos < version_pos,
            "ordering install < marker < version: {s}"
        );
        // The marker is gated behind `&&` so it only prints on install success.
        assert!(
            s.contains("&& printf"),
            "marker gated on install success: {s}"
        );
    }

    /// The v0.9.0 regression: install succeeds (stdout carries the apk
    /// success banner) yet the version-check fails with exit 127 (glibc binary
    /// on musl). The detail must attribute the failure to the version-check,
    /// include `exit 127`, surface the real stderr, and NOT show the install
    /// banner as the "error".
    #[cfg(unix)]
    #[test]
    fn smoke_failure_detail_attributes_version_check_and_surfaces_exit() {
        let stdout = format!(
            "(1/1) Installing anodizer (0.9.0-r1)\nExecuting busybox trigger\nOK: 17.8 MiB in 17 packages\n{SMOKE_STEP_MARKER}\n"
        );
        let stderr = "sh: anodizer: not found";
        let out = fake_output(127, &stdout, stderr);
        let detail = smoke_failure_detail("anodizer", &out);

        assert!(
            detail.contains("version-check"),
            "must attribute to the version-check step: {detail}"
        );
        assert!(
            detail.contains("exit 127"),
            "must include the exit code: {detail}"
        );
        assert!(
            detail.contains("anodizer: not found"),
            "must surface the real stderr: {detail}"
        );
        // The install success banner must NOT masquerade as the failure detail.
        assert!(
            !detail.contains("OK: 17.8 MiB"),
            "install success banner must not mask the real error: {detail}"
        );
        assert!(
            !detail.starts_with("install step failed"),
            "must not blame the install step: {detail}"
        );
    }

    /// A Docker-daemon failure to start the container (image pull / exec /
    /// OCI runtime error) must be labeled "container failed to start", NOT
    /// "install step failed" — the container never reached the install step.
    /// The real docker stderr must still surface.
    #[cfg(unix)]
    #[test]
    fn smoke_failure_detail_labels_container_start_failure() {
        let stderr = "docker: Error response from daemon: manifest for alpine:latest not found: manifest unknown.";
        let out = fake_output(125, "", stderr);
        let detail = smoke_failure_detail("anodizer", &out);
        assert!(
            detail.starts_with("container failed to start"),
            "daemon error must be labeled container-start, not install: {detail}"
        );
        assert!(
            !detail.contains("install step failed"),
            "must not blame the install step: {detail}"
        );
        assert!(
            detail.contains("exit 125"),
            "must include the exit code: {detail}"
        );
        assert!(
            detail.contains("manifest unknown"),
            "must surface the real docker error: {detail}"
        );
    }

    /// A package that echoes the marker token MID-LINE during a failing install
    /// must NOT be mistaken for the install-success signal: only an own-line
    /// marker (the position our `printf` writes it) counts. The failure must
    /// still be attributed to the install step.
    #[cfg(unix)]
    #[test]
    fn smoke_failure_detail_ignores_mid_line_marker_forge() {
        let stdout = format!("noise before {SMOKE_STEP_MARKER} noise after\ninstall failed here\n");
        let out = fake_output(1, &stdout, "apk: broken package");
        let detail = smoke_failure_detail("anodizer", &out);
        assert!(
            detail.starts_with("install step failed"),
            "a mid-line marker must not forge install-success: {detail}"
        );
        assert!(
            !detail.contains("version-check"),
            "must not be attributed to the version-check: {detail}"
        );
    }

    #[test]
    fn marker_present_requires_own_line() {
        assert!(marker_present(&format!("a\n{SMOKE_STEP_MARKER}\nb")));
        assert!(marker_present(SMOKE_STEP_MARKER));
        // Embedded mid-line ⇒ not a valid marker.
        assert!(!marker_present(&format!(
            "prefix {SMOKE_STEP_MARKER} suffix"
        )));
        assert!(!marker_present("no marker at all"));
    }

    /// When install itself fails the marker is absent; the detail must blame
    /// the install step, include the exit code, and surface the install error.
    #[cfg(unix)]
    #[test]
    fn smoke_failure_detail_attributes_install_when_marker_absent() {
        let out = fake_output(
            1,
            "Installing...\n",
            "ERROR: unable to select packages: conflicts",
        );
        let detail = smoke_failure_detail("anodizer", &out);
        assert!(
            detail.starts_with("install step failed"),
            "must blame the install step when the marker is absent: {detail}"
        );
        assert!(
            detail.contains("exit 1"),
            "must include the exit code: {detail}"
        );
        assert!(
            detail.contains("conflicts"),
            "must surface the install error: {detail}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_detail_always_includes_exit_code() {
        // A step that succeeds noisily on stdout but exits non-zero (no
        // stderr): the exit code must still surface.
        let out = fake_output(2, "some progress output", "");
        let detail = output_detail(&out);
        assert!(detail.contains("exit 2"), "{detail}");
        assert!(detail.contains("some progress output"), "{detail}");
    }

    #[test]
    fn tail_truncates_long_streams_keeping_the_end() {
        let long = format!("HEAD{}TAILMARKER", "x".repeat(4096));
        let t = tail(&long);
        assert!(t.contains("TAILMARKER"), "keeps the end: {}", &t[..40]);
        assert!(t.contains("truncated"), "marks truncation");
        assert!(!t.contains("HEAD"), "drops the head");
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
            platform: None,
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
