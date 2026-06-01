//! Partial build target resolution for split/merge CI fan-out.
//!
//! Equivalent to GoReleaser Pro's `partial.Pipe` — resolves which build targets
//! to include when running in split mode.

use anyhow::{Context as _, Result};

use crate::EnvSource;
use crate::config::PartialConfig;
use crate::target;

// ---------------------------------------------------------------------------
// PartialTarget — resolved target filter
// ---------------------------------------------------------------------------

/// A resolved partial build target filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartialTarget {
    /// Exact target triple match (e.g., `x86_64-unknown-linux-gnu`).
    Exact(String),
    /// Match by OS (and optionally arch) components.
    OsArch { os: String, arch: Option<String> },
    /// Restrict to an explicit list of target triples. Used by the
    /// Determinism Harness and `release --targets=<csv>` to drive
    /// platform-sharded rebuilds: the build stage retains only those
    /// configured targets that intersect the supplied list, leaving the
    /// remaining cross-shard targets to sibling jobs.
    Targets(Vec<String>),
}

impl PartialTarget {
    /// Filter a list of target triples to those matching this partial target.
    pub fn filter_targets(&self, targets: &[String]) -> Vec<String> {
        match self {
            PartialTarget::Exact(t) => targets.iter().filter(|tt| *tt == t).cloned().collect(),
            PartialTarget::OsArch { os, arch } => targets
                .iter()
                .filter(|tt| {
                    let (t_os, t_arch) = target::map_target(tt);
                    t_os == *os && arch.as_ref().is_none_or(|a| t_arch == *a)
                })
                .cloned()
                .collect(),
            PartialTarget::Targets(list) => targets
                .iter()
                .filter(|tt| list.iter().any(|wanted| wanted == *tt))
                .cloned()
                .collect(),
        }
    }

    /// Return the dist subdirectory name for this partial target.
    /// - `Exact("x86_64-unknown-linux-gnu")` → `"x86_64-unknown-linux-gnu"`
    /// - `OsArch { os: "linux", arch: None }` → `"linux"`
    /// - `OsArch { os: "linux", arch: Some("amd64") }` → `"linux_amd64"`
    /// - `Targets(["x86_64-...", "aarch64-..."])` → `"targets-x86_64-..."` (first triple)
    ///
    /// # Divergence from GoReleaser
    ///
    /// GoReleaser Pro writes split shards to `dist/$GOOS` (or
    /// `dist/$GOOS_$GOARCH` when `partial.by: target`). Anodizer
    /// matches that shape for the `OsArch` variant — `OsArch { os:
    /// "linux", arch: None }` resolves to `"linux"`, identical to GR's
    /// `dist/linux` — but the `Exact` variant uses the full Rust target
    /// triple instead of GR's Go-style `<goos>_<goarch>` (because the
    /// triple is the natural granularity for Rust toolchains), and the
    /// `Targets` variant is anodizer-only (drives the determinism
    /// harness's sharded matrix, not user-facing CI fan-out).
    ///
    /// Practical consequence: split shards produced by anodizer cannot
    /// be merged by `goreleaser` and vice versa. Anodizer's CLI does
    /// not attempt cross-tool interop; the subdir name is purely
    /// internal to the per-tool merge step.
    ///
    /// ```
    /// use anodizer_core::partial::PartialTarget;
    ///
    /// // OsArch matches GR's `dist/linux` shape exactly.
    /// assert_eq!(
    ///     PartialTarget::OsArch { os: "linux".into(), arch: None }.dist_subdir(),
    ///     "linux",
    /// );
    ///
    /// // Exact uses the full Rust triple (not GR's `linux_amd64`).
    /// assert_eq!(
    ///     PartialTarget::Exact("x86_64-unknown-linux-gnu".into()).dist_subdir(),
    ///     "x86_64-unknown-linux-gnu",
    /// );
    /// ```
    pub fn dist_subdir(&self) -> String {
        match self {
            PartialTarget::Exact(t) => t.clone(),
            PartialTarget::OsArch { os, arch } => {
                if let Some(a) = arch {
                    format!("{}_{}", os, a)
                } else {
                    os.clone()
                }
            }
            PartialTarget::Targets(list) => {
                // Deterministic name derived from the first triple. This
                // is only consulted by `--split`/`--merge` for split-
                // artifact directory naming; the harness path does not
                // round-trip through `dist/<subdir>/context.json`.
                match list.first() {
                    Some(first) => format!("targets-{}", first),
                    None => "targets-empty".to_string(),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Target resolution — env vars → host detection
// ---------------------------------------------------------------------------

/// Resolve the partial build target from environment variables and config.
///
/// Priority chain (matching GoReleaser Pro's approach):
/// 1. `TARGET` env var — exact target triple (highest priority)
/// 2. `ANODIZER_OS`/`ANODIZER_ARCH` (canonical) or `GGOOS`/`GGOARCH` (GoReleaser
///    alias; filter-only — does not override the host's `GOOS`/`GOARCH` for hooks)
/// 3. Host detection via `rustc -vV`, interpreted per `partial.by` config
pub fn resolve_partial_target(config: &Option<PartialConfig>) -> Result<PartialTarget> {
    resolve_partial_target_with_env(config, &crate::ProcessEnvSource)
}

/// Env-injectable form of [`resolve_partial_target`]. Production wires up
/// [`ProcessEnvSource`]; tests inject a
/// [`MapEnvSource`](crate::MapEnvSource) to drive the env-var branches
/// without mutating the process env.
pub fn resolve_partial_target_with_env<E: EnvSource + ?Sized>(
    config: &Option<PartialConfig>,
    env: &E,
) -> Result<PartialTarget> {
    // Priority 1: TARGET env var — exact target triple
    if let Some(t) = env.var("TARGET")
        && !t.is_empty()
    {
        return Ok(PartialTarget::Exact(t));
    }

    // Priority 2: ANODIZER_OS/ANODIZER_ARCH, or GGOOS/GGOARCH alias for GoReleaser
    // compatibility. Canonical vars win when both are set.
    let os = env
        .var("ANODIZER_OS")
        .filter(|s| !s.is_empty())
        .or_else(|| env.var("GGOOS").filter(|s| !s.is_empty()));
    if let Some(os) = os {
        let arch = env
            .var("ANODIZER_ARCH")
            .filter(|a| !a.is_empty())
            .or_else(|| env.var("GGOARCH").filter(|a| !a.is_empty()));
        return Ok(PartialTarget::OsArch { os, arch });
    }

    // Priority 3: host detection, interpreted per partial.by
    let host = detect_host_target()?;
    let by = config
        .as_ref()
        .and_then(|c| c.by.as_deref())
        .unwrap_or("os");

    match by {
        "os" => {
            let (os, _) = target::map_target(&host);
            Ok(PartialTarget::OsArch { os, arch: None })
        }
        "target" => Ok(PartialTarget::Exact(host)),
        other => anyhow::bail!(
            "partial.by: unknown value '{}' (expected 'os' or 'target')",
            other
        ),
    }
}

/// Spawn `rustc -vV` once and return its stdout.
///
/// Both [`detect_host_target`] (the `host:` line) and
/// [`detect_rustc_version`] (the `release:` line) parse the same `rustc -vV`
/// block, so the spawn is centralized here to avoid invoking rustc twice in a
/// single build/release run. Returns the raw stdout as a `String`.
fn run_rustc_vv() -> Result<String> {
    let mut cmd = std::process::Command::new("rustc");
    cmd.args(["-vV"]);
    tracing::debug!(args = ?cmd.get_args(), "spawning rustc -vV for host/version detection");
    let output = cmd.output().context("failed to run `rustc -vV`")?;

    if !output.status.success() {
        anyhow::bail!(
            "rustc -vV failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Extract the `host:` target triple from a `rustc -vV` output block.
pub(crate) fn parse_host_from_output(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("host: ").map(|h| h.trim().to_string()))
}

/// Extract the `release:` version (e.g. `"1.96.0"`) from a `rustc -vV` block.
pub(crate) fn parse_rustc_version_from_output(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("release: ").map(|v| v.trim().to_string()))
}

/// Detect the host target triple via `rustc -vV`.
pub fn detect_host_target() -> Result<String> {
    let stdout = run_rustc_vv()?;
    parse_host_from_output(&stdout).context("could not detect host target from `rustc -vV` output")
}

/// Detect the rustc release version string via `rustc -vV`.
///
/// Parses the `release:` line from `rustc -vV` output (e.g. `"1.96.0"`).
/// Returns `None` gracefully when rustc is unavailable, the command fails,
/// or the line is absent — callers treat a missing version as an empty string.
pub fn detect_rustc_version() -> Option<String> {
    let stdout = run_rustc_vv().ok()?;
    parse_rustc_version_from_output(&stdout)
}

/// Resolve the effective host target triple for `--single-target`.
///
/// Priority chain (mirrors GoReleaser's `partial.Pipe.Run` +
/// `getGoEnvFilter` / `findRuntime` so a config originally written for GR
/// keeps the same CI escape hatches under anodizer):
/// 1. `TARGET=<triple>` env var (exact triple, highest priority).
/// 2. `GGOOS` / `GGOARCH` filter-only aliases combined with the host
///    triple to synthesize a `<arch>-...-<os>...` shape that
///    [`find_runtime_target`] can later match against configured targets.
///    These do NOT bleed into hook subprocesses' `GOOS` / `GOARCH`.
/// 3. `rustc -vV` host detection.
///
/// `GGOOS`/`GGOARCH` are honored on a best-effort basis — without a real
/// `GOOS` -> rust-triple mapping the synthesized string is only useful
/// when paired with the alias-table fallback in
/// [`find_runtime_target`]. When both env vars are absent the resolver
/// returns the raw `rustc -vV` host triple.
pub fn resolve_host_target_with_env<E: EnvSource + ?Sized>(env: &E) -> Result<String> {
    // Priority 1: TARGET env var - exact triple override.
    if let Some(t) = env.var("TARGET")
        && !t.trim().is_empty()
    {
        return Ok(t);
    }

    // Priority 2 + 3: detect the host first; if `GGOOS`/`GGOARCH` were
    // supplied as filter-only overrides, rewrite the OS/arch components
    // of the host triple so downstream filtering picks up the override.
    let host = detect_host_target()?;
    let ggoos = env.var("GGOOS").filter(|s| !s.trim().is_empty());
    let ggoarch = env.var("GGOARCH").filter(|s| !s.trim().is_empty());
    if ggoos.is_some() || ggoarch.is_some() {
        return Ok(synthesize_triple_with_overrides(
            &host,
            ggoos.as_deref(),
            ggoarch.as_deref(),
        ));
    }
    Ok(host)
}

/// Process-env form of [`resolve_host_target_with_env`].
pub fn resolve_host_target() -> Result<String> {
    resolve_host_target_with_env(&crate::ProcessEnvSource)
}

/// Best-effort host->triple fuzzy matcher for `--single-target`.
///
/// Mirrors GoReleaser's `partial.findRuntime` (OSS): walks
/// `goos -> {macos, darwin}` and `goarch -> {x86_64, amd64, arm64,
/// aarch64, 386 -> i686/i586/i386}` alias tables to find a configured
/// target that matches the runtime even when the user's `targets:`
/// list spells a semantically-equivalent but lexically-different triple
/// than `rustc -vV`. Returns the first configured target whose
/// `(os, arch)` (via [`crate::target::map_target`]) matches the host
/// after alias normalization, or `None` when nothing matches.
pub fn find_runtime_target(host: &str, configured: &[String]) -> Option<String> {
    let (host_os, host_arch) = crate::target::map_target(host);
    configured
        .iter()
        .find(|t| {
            let (t_os, t_arch) = crate::target::map_target(t);
            t_os == host_os && t_arch == host_arch
        })
        .cloned()
}

/// Replace the OS / arch first-component of `host_triple` with the
/// supplied `GGOOS` / `GGOARCH` overrides so the synthesized string
/// passes through [`find_runtime_target`] correctly.
///
/// The mapping accepts both GoReleaser-style aliases (`darwin`, `amd64`,
/// `arm64`, `386`) and their rust-triple spellings (`apple-darwin`,
/// `x86_64`, `aarch64`, `i686`). Unknown values are passed through
/// verbatim — best-effort behaviour matching `findRuntime`.
fn synthesize_triple_with_overrides(
    host_triple: &str,
    goos: Option<&str>,
    goarch: Option<&str>,
) -> String {
    // Map alias -> canonical rust component when we recognize it.
    let arch_token = goarch.map(|a| match a {
        "amd64" | "x86_64" => "x86_64",
        "arm64" | "aarch64" => "aarch64",
        "386" | "i686" => "i686",
        other => other,
    });
    let os_token = goos.map(|o| match o {
        "darwin" | "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        "windows" => "pc-windows-msvc",
        other => other,
    });

    // Pull the original components apart.
    let parts: Vec<&str> = host_triple.split('-').collect();
    let original_arch = parts.first().copied().unwrap_or("");
    let original_rest = if parts.len() > 1 {
        parts[1..].join("-")
    } else {
        String::new()
    };

    let new_arch = arch_token.unwrap_or(original_arch);
    let new_rest = os_token.map(str::to_string).unwrap_or(original_rest);

    if new_rest.is_empty() {
        new_arch.to_string()
    } else {
        format!("{}-{}", new_arch, new_rest)
    }
}

// ---------------------------------------------------------------------------
// Host-buildable target filtering (--host-targets)
// ---------------------------------------------------------------------------

/// Why a configured target cannot be built on the current host. Each
/// variant maps to a cross-compile case anodizer's cargo-zigbuild path
/// cannot satisfy off the corresponding native host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostConstraint {
    /// Apple (`*-apple-*`, including `*-apple-darwin` and iOS) targets need
    /// the macOS SDK (Security / CoreFoundation frameworks) only present on
    /// a real Mac. cargo-zigbuild cannot synthesize it.
    NeedsAppleHost,
    /// Windows-MSVC (`*-windows-msvc`) targets need the MSVC SDK / CRT
    /// headers (e.g. `assert.h`) that cargo-zigbuild does not bundle; only a
    /// Windows host has them. `*-windows-gnu` is unaffected (zig ships the
    /// MinGW runtime) and builds from any host.
    NeedsWindowsHost,
}

impl HostConstraint {
    /// Human-readable clause naming the host this constraint requires, used
    /// to build the loud skip message and the empty-result hard error.
    fn reason(self) -> &'static str {
        match self {
            HostConstraint::NeedsAppleHost => "apple targets require a macOS host",
            HostConstraint::NeedsWindowsHost => "windows-msvc targets require a Windows host",
        }
    }
}

/// Classify why `triple` is not buildable on `host`, or `None` when the host
/// can build it.
///
/// A target needs a specific host when it is an apple target on a non-apple
/// host, or a windows-msvc target on a non-windows host. Everything else
/// (linux gnu/musl, `*-windows-gnu`, ...) is cross-buildable from any host.
fn target_host_constraint(host: &str, triple: &str) -> Option<HostConstraint> {
    if crate::target::is_darwin(triple) && !host_is_apple(host) {
        Some(HostConstraint::NeedsAppleHost)
    } else if crate::target::is_windows_msvc(triple) && !host_is_windows(host) {
        Some(HostConstraint::NeedsWindowsHost)
    } else {
        None
    }
}

/// `true` when the host triple is an Apple/Darwin host (and can therefore
/// build Apple targets in addition to everything else).
pub fn host_is_apple(host: &str) -> bool {
    crate::target::is_darwin(host)
}

/// `true` when the host triple is a Windows host (and can therefore build
/// windows-msvc targets in addition to everything else).
pub fn host_is_windows(host: &str) -> bool {
    crate::target::is_windows(host)
}

/// Partition `configured` targets into `(buildable, skipped)` for the
/// given host triple, per the `--host-targets` rule.
///
/// Every configured target is kept EXCEPT those that need a native host the
/// current host is not:
/// - apple (`*-apple-*`) targets are skipped off a non-Apple host (they need
///   the macOS SDK only present on a real Mac), and
/// - windows-msvc (`*-windows-msvc`) targets are skipped off a non-Windows
///   host (they need the MSVC SDK / CRT headers cargo-zigbuild does not bundle).
///
/// Linux (gnu/musl), `*-windows-gnu`, and all other targets are kept from any
/// host (cargo-zigbuild cross-links them). Order is preserved within each
/// partition.
///
/// ```
/// use anodizer_core::partial::host_buildable_targets;
///
/// let configured = vec![
///     "x86_64-unknown-linux-gnu".to_string(),
///     "x86_64-pc-windows-gnu".to_string(),
///     "x86_64-pc-windows-msvc".to_string(),
///     "x86_64-apple-darwin".to_string(),
/// ];
/// let (kept, skipped) =
///     host_buildable_targets("x86_64-unknown-linux-gnu", &configured);
/// assert_eq!(
///     kept,
///     vec!["x86_64-unknown-linux-gnu", "x86_64-pc-windows-gnu"],
/// );
/// assert_eq!(
///     skipped,
///     vec!["x86_64-pc-windows-msvc", "x86_64-apple-darwin"],
/// );
/// ```
pub fn host_buildable_targets(host: &str, configured: &[String]) -> (Vec<String>, Vec<String>) {
    let mut kept = Vec::new();
    let mut skipped = Vec::new();
    for t in configured {
        if target_host_constraint(host, t).is_some() {
            skipped.push(t.clone());
        } else {
            kept.push(t.clone());
        }
    }
    (kept, skipped)
}

/// Render the single loud-log line emitted when `--host-targets` skips
/// configured targets, naming the host OS, the count, and — grouped by
/// reason — which triples were skipped and why. Returns `None` when nothing
/// was skipped.
///
/// Example:
/// `host-targets: skipping 3 target(s) not buildable on this linux host:
/// aarch64-apple-darwin, x86_64-apple-darwin (apple targets require a macOS
/// host); x86_64-pc-windows-msvc (windows-msvc targets require a Windows host)`
pub fn host_targets_skip_message(host: &str, skipped: &[String]) -> Option<String> {
    if skipped.is_empty() {
        return None;
    }
    let (host_os, _) = crate::target::map_target(host);
    Some(format!(
        "host-targets: skipping {} target(s) not buildable on this {} host: {}",
        skipped.len(),
        host_os,
        host_targets_skip_reasons(host, skipped),
    ))
}

/// Group `skipped` triples by their host constraint and render
/// `<triples> (<reason>)` clauses joined by `; `, preserving the apple →
/// windows order so the message is deterministic. Each triple is attributed
/// to the constraint that caused it to be skipped.
///
/// Consumed by the loud skip line and by the `--host-targets` empty-result
/// hard error (where every configured target was skipped), so the error
/// names the native host each group needs rather than a hardcoded remedy.
pub fn host_targets_skip_reasons(host: &str, skipped: &[String]) -> String {
    [
        HostConstraint::NeedsAppleHost,
        HostConstraint::NeedsWindowsHost,
    ]
    .into_iter()
    .filter_map(|constraint| {
        let triples: Vec<&str> = skipped
            .iter()
            .filter(|t| target_host_constraint(host, t) == Some(constraint))
            .map(String::as_str)
            .collect();
        if triples.is_empty() {
            None
        } else {
            Some(format!("{} ({})", triples.join(", "), constraint.reason()))
        }
    })
    .collect::<Vec<_>>()
    .join("; ")
}

/// Suggest a GitHub Actions runner for a given OS.
pub fn suggest_runner(os: &str) -> &'static str {
    match os {
        "linux" => "ubuntu-latest",
        "darwin" => "macos-latest",
        "windows" => "windows-latest",
        _ => "ubuntu-latest", // cross-compile
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PartialConfig;
    use serial_test::serial;

    // -----------------------------------------------------------------------
    // PartialTarget filtering
    // -----------------------------------------------------------------------

    #[test]
    fn test_exact_filter_matches_one() {
        let target = PartialTarget::Exact("x86_64-unknown-linux-gnu".to_string());
        let targets = vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
            "x86_64-apple-darwin".to_string(),
        ];
        let filtered = target.filter_targets(&targets);
        assert_eq!(filtered, vec!["x86_64-unknown-linux-gnu"]);
    }

    #[test]
    fn test_exact_filter_no_match() {
        let target = PartialTarget::Exact("riscv64gc-unknown-linux-gnu".to_string());
        let targets = vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-apple-darwin".to_string(),
        ];
        let filtered = target.filter_targets(&targets);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_os_filter_matches_all_linux() {
        let target = PartialTarget::OsArch {
            os: "linux".to_string(),
            arch: None,
        };
        let targets = vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
            "x86_64-apple-darwin".to_string(),
            "x86_64-pc-windows-msvc".to_string(),
        ];
        let filtered = target.filter_targets(&targets);
        assert_eq!(
            filtered,
            vec!["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",]
        );
    }

    #[test]
    fn test_os_arch_filter() {
        let target = PartialTarget::OsArch {
            os: "linux".to_string(),
            arch: Some("arm64".to_string()),
        };
        let targets = vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
        ];
        let filtered = target.filter_targets(&targets);
        assert_eq!(filtered, vec!["aarch64-unknown-linux-gnu"]);
    }

    #[test]
    fn test_os_filter_darwin() {
        let target = PartialTarget::OsArch {
            os: "darwin".to_string(),
            arch: None,
        };
        let targets = vec![
            "x86_64-apple-darwin".to_string(),
            "aarch64-apple-darwin".to_string(),
            "x86_64-unknown-linux-gnu".to_string(),
        ];
        let filtered = target.filter_targets(&targets);
        assert_eq!(
            filtered,
            vec!["x86_64-apple-darwin", "aarch64-apple-darwin"]
        );
    }

    #[test]
    fn test_os_filter_windows() {
        let target = PartialTarget::OsArch {
            os: "windows".to_string(),
            arch: None,
        };
        let targets = vec![
            "x86_64-pc-windows-msvc".to_string(),
            "aarch64-pc-windows-msvc".to_string(),
            "x86_64-unknown-linux-gnu".to_string(),
        ];
        let filtered = target.filter_targets(&targets);
        assert_eq!(
            filtered,
            vec!["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"]
        );
    }

    // -----------------------------------------------------------------------
    // Dist subdirectory naming
    // -----------------------------------------------------------------------

    #[test]
    fn test_dist_subdir_exact() {
        let target = PartialTarget::Exact("x86_64-unknown-linux-gnu".to_string());
        assert_eq!(target.dist_subdir(), "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn test_dist_subdir_os_only() {
        let target = PartialTarget::OsArch {
            os: "linux".to_string(),
            arch: None,
        };
        assert_eq!(target.dist_subdir(), "linux");
    }

    #[test]
    fn test_dist_subdir_os_arch() {
        let target = PartialTarget::OsArch {
            os: "linux".to_string(),
            arch: Some("amd64".to_string()),
        };
        assert_eq!(target.dist_subdir(), "linux_amd64");
    }

    /// `OsArch { os: "linux", arch: None }` must spell `"linux"` —
    /// byte-for-byte the same name GoReleaser writes to `dist/linux`.
    /// This is the only `dist_subdir` shape that round-trips between
    /// the two tools and is therefore worth pinning explicitly.
    #[test]
    fn dist_subdir_os_only_matches_goreleaser_layout() {
        let target = PartialTarget::OsArch {
            os: "linux".to_string(),
            arch: None,
        };
        assert_eq!(target.dist_subdir(), "linux");
    }

    /// The `Exact` variant uses the full Rust target triple, which
    /// diverges from GoReleaser's `dist/$GOOS_$GOARCH` shape. Lock
    /// the anodizer-specific spelling in so the rustdoc-documented
    /// divergence is also enforced by a test.
    #[test]
    fn dist_subdir_exact_uses_full_rust_triple_not_goos_goarch() {
        let target = PartialTarget::Exact("x86_64-unknown-linux-gnu".to_string());
        assert_eq!(target.dist_subdir(), "x86_64-unknown-linux-gnu");
        assert_ne!(target.dist_subdir(), "linux_amd64");
    }

    // -----------------------------------------------------------------------
    // PartialTarget::Targets — explicit triple list (sharded build / harness)
    // -----------------------------------------------------------------------

    #[test]
    fn test_targets_filter_matches_intersection() {
        let target = PartialTarget::Targets(vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
        ]);
        let configured = vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
            "x86_64-apple-darwin".to_string(),
            "aarch64-apple-darwin".to_string(),
        ];
        let filtered = target.filter_targets(&configured);
        assert_eq!(
            filtered,
            vec!["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"]
        );
    }

    #[test]
    fn test_targets_filter_drops_non_configured_entries() {
        // Triples requested but not configured are simply absent from the
        // result — `filter_targets` is intersection, not union.
        let target = PartialTarget::Targets(vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "x86_64-pc-windows-msvc".to_string(),
        ]);
        let configured = vec!["x86_64-unknown-linux-gnu".to_string()];
        let filtered = target.filter_targets(&configured);
        assert_eq!(filtered, vec!["x86_64-unknown-linux-gnu"]);
    }

    #[test]
    fn test_targets_filter_empty_list_yields_empty() {
        let target = PartialTarget::Targets(Vec::new());
        let configured = vec!["x86_64-unknown-linux-gnu".to_string()];
        assert!(target.filter_targets(&configured).is_empty());
    }

    #[test]
    fn test_dist_subdir_targets_uses_first_triple() {
        let target = PartialTarget::Targets(vec![
            "x86_64-apple-darwin".to_string(),
            "aarch64-apple-darwin".to_string(),
        ]);
        assert_eq!(target.dist_subdir(), "targets-x86_64-apple-darwin");
    }

    #[test]
    fn test_dist_subdir_targets_empty_list_has_stable_name() {
        let target = PartialTarget::Targets(Vec::new());
        assert_eq!(target.dist_subdir(), "targets-empty");
    }

    // -----------------------------------------------------------------------
    // Host detection
    // -----------------------------------------------------------------------

    #[test]
    fn test_detect_host_target() {
        // This test runs on whatever machine the test suite runs on.
        // It should always succeed if rustc is available.
        let host = detect_host_target().unwrap();
        assert!(!host.is_empty());
        // Should contain at least one hyphen (target triple format)
        assert!(host.contains('-'), "host triple should contain '-': {host}");
    }

    // -----------------------------------------------------------------------
    // resolve_partial_target (without env vars — tests host fallback)
    // -----------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_resolve_with_os_default() {
        // Clear env vars that might interfere
        // SAFETY: test-only, no concurrent env var access in these serial tests
        unsafe {
            std::env::remove_var("TARGET");
            std::env::remove_var("ANODIZER_OS");
            std::env::remove_var("ANODIZER_ARCH");
        }

        let config = None; // defaults to "os"
        let target = resolve_partial_target(&config).unwrap();

        // Should be an OsArch with the host's OS
        match target {
            PartialTarget::OsArch { os, arch } => {
                assert!(!os.is_empty());
                assert!(arch.is_none()); // os mode doesn't set arch
            }
            other => panic!("expected OsArch, got: {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn test_resolve_with_by_target() {
        // SAFETY: test-only, no concurrent env var access in these serial tests
        unsafe {
            std::env::remove_var("TARGET");
            std::env::remove_var("ANODIZER_OS");
            std::env::remove_var("ANODIZER_ARCH");
        }

        let config = Some(PartialConfig {
            by: Some("target".to_string()),
        });
        let target = resolve_partial_target(&config).unwrap();

        // Should be an Exact match with the full host triple
        match target {
            PartialTarget::Exact(t) => {
                assert!(t.contains('-'), "should be full triple: {t}");
            }
            other => panic!("expected Exact, got: {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn test_resolve_invalid_by_value() {
        // SAFETY: test-only, no concurrent env var access in these serial tests
        unsafe {
            std::env::remove_var("TARGET");
            std::env::remove_var("ANODIZER_OS");
            std::env::remove_var("ANODIZER_ARCH");
        }

        let config = Some(PartialConfig {
            by: Some("invalid".to_string()),
        });
        let err = resolve_partial_target(&config).unwrap_err();
        assert!(err.to_string().contains("unknown value"), "got: {}", err);
    }

    #[test]
    #[serial]
    fn test_resolve_by_os_works_and_legacy_goos_rejected() {
        // SAFETY: test-only, no concurrent env var access in these serial tests
        unsafe {
            std::env::remove_var("TARGET");
            std::env::remove_var("ANODIZER_OS");
            std::env::remove_var("ANODIZER_ARCH");
        }

        let ok = resolve_partial_target(&Some(PartialConfig {
            by: Some("os".to_string()),
        }))
        .unwrap();
        assert!(matches!(ok, PartialTarget::OsArch { arch: None, .. }));

        // The Go-named `goos` value was hard-renamed to `os`; the old
        // spelling must no longer resolve.
        let err = resolve_partial_target(&Some(PartialConfig {
            by: Some("goos".to_string()),
        }))
        .unwrap_err();
        assert!(err.to_string().contains("unknown value"), "got: {}", err);
    }

    // -----------------------------------------------------------------------
    // Runner suggestion
    // -----------------------------------------------------------------------

    #[test]
    fn test_suggest_runner() {
        assert_eq!(suggest_runner("linux"), "ubuntu-latest");
        assert_eq!(suggest_runner("darwin"), "macos-latest");
        assert_eq!(suggest_runner("windows"), "windows-latest");
        assert_eq!(suggest_runner("freebsd"), "ubuntu-latest");
    }

    // -----------------------------------------------------------------------
    // resolve_host_target_with_env (--single-target path)
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_host_target_honours_target_env_override() {
        let env = crate::MapEnvSource::new().with("TARGET", "x86_64-unknown-linux-musl");
        let triple = resolve_host_target_with_env(&env).unwrap();
        assert_eq!(triple, "x86_64-unknown-linux-musl");
    }

    #[test]
    fn resolve_host_target_target_env_wins_over_ggoos() {
        let env = crate::MapEnvSource::new()
            .with("TARGET", "aarch64-apple-darwin")
            .with("GGOOS", "linux")
            .with("GGOARCH", "amd64");
        let triple = resolve_host_target_with_env(&env).unwrap();
        assert_eq!(triple, "aarch64-apple-darwin");
    }

    #[test]
    fn resolve_host_target_blank_target_falls_through() {
        // A whitespace-only TARGET should be ignored (matches GR's
        // `if t := os.Getenv("TARGET"); t != ""` early-return).
        let env = crate::MapEnvSource::new().with("TARGET", "   ");
        let triple = resolve_host_target_with_env(&env).unwrap();
        assert!(triple.contains('-'), "fell back to rustc -vV: {triple}");
    }

    #[test]
    fn ggoos_overrides_host_os_component() {
        // No TARGET set; GGOOS=darwin should rewrite the host triple's
        // OS slot to `apple-darwin`.
        let synthesized = synthesize_triple_with_overrides(
            "x86_64-unknown-linux-gnu",
            Some("darwin"),
            Some("arm64"),
        );
        assert_eq!(synthesized, "aarch64-apple-darwin");
    }

    #[test]
    fn ggoos_alone_keeps_host_arch() {
        let synthesized =
            synthesize_triple_with_overrides("x86_64-unknown-linux-gnu", Some("windows"), None);
        assert_eq!(synthesized, "x86_64-pc-windows-msvc");
    }

    #[test]
    fn ggoarch_alone_keeps_host_os() {
        let synthesized =
            synthesize_triple_with_overrides("x86_64-unknown-linux-gnu", None, Some("arm64"));
        assert_eq!(synthesized, "aarch64-unknown-linux-gnu");
    }

    // -----------------------------------------------------------------------
    // find_runtime_target (host-alias fallback for --single-target)
    // -----------------------------------------------------------------------

    #[test]
    fn find_runtime_matches_exact() {
        let configured = vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-apple-darwin".to_string(),
        ];
        let m = find_runtime_target("x86_64-unknown-linux-gnu", &configured);
        assert_eq!(m.as_deref(), Some("x86_64-unknown-linux-gnu"));
    }

    #[test]
    fn find_runtime_matches_by_alias() {
        // Host says `x86_64-unknown-linux-musl`; configured target uses
        // `x86_64-unknown-linux-gnu`. Both map to `(linux, amd64)` so
        // the alias matcher should pair them.
        let configured = vec!["x86_64-unknown-linux-gnu".to_string()];
        let m = find_runtime_target("x86_64-unknown-linux-musl", &configured);
        assert_eq!(m.as_deref(), Some("x86_64-unknown-linux-gnu"));
    }

    #[test]
    fn find_runtime_returns_none_when_no_match() {
        let configured = vec!["aarch64-apple-darwin".to_string()];
        let m = find_runtime_target("x86_64-unknown-linux-gnu", &configured);
        assert!(m.is_none());
    }

    // -----------------------------------------------------------------------
    // host_buildable_targets (--host-targets)
    // -----------------------------------------------------------------------

    const LINUX_HOST: &str = "x86_64-unknown-linux-gnu";
    const MAC_HOST: &str = "aarch64-apple-darwin";
    const WINDOWS_HOST: &str = "x86_64-pc-windows-msvc";

    /// Full cross-host fixture: 2 linux, 1 windows-gnu, 1 windows-msvc, 2
    /// apple. Exercises every classification branch.
    fn mixed_targets() -> Vec<String> {
        vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
            "x86_64-pc-windows-gnu".to_string(),
            "x86_64-pc-windows-msvc".to_string(),
            "x86_64-apple-darwin".to_string(),
            "aarch64-apple-darwin".to_string(),
        ]
    }

    #[test]
    fn host_buildable_linux_keeps_cross_buildable_skips_apple_and_msvc() {
        let (kept, skipped) = host_buildable_targets(LINUX_HOST, &mixed_targets());
        assert_eq!(
            kept,
            vec![
                "x86_64-unknown-linux-gnu",
                "aarch64-unknown-linux-gnu",
                "x86_64-pc-windows-gnu",
            ],
            "linux + windows-gnu targets are cross-buildable from a linux host"
        );
        assert_eq!(
            skipped,
            vec![
                "x86_64-pc-windows-msvc",
                "x86_64-apple-darwin",
                "aarch64-apple-darwin",
            ],
            "windows-msvc (needs Windows) and apple (needs macOS) are skipped on linux"
        );
    }

    #[test]
    fn host_buildable_apple_host_keeps_apple_still_skips_msvc() {
        // A macOS host builds apple targets, but windows-msvc still needs a
        // Windows host — msvc can't be cross-built even from a Mac.
        let (kept, skipped) = host_buildable_targets(MAC_HOST, &mixed_targets());
        assert_eq!(
            kept,
            vec![
                "x86_64-unknown-linux-gnu",
                "aarch64-unknown-linux-gnu",
                "x86_64-pc-windows-gnu",
                "x86_64-apple-darwin",
                "aarch64-apple-darwin",
            ],
            "apple host keeps apple + linux + windows-gnu: {kept:?}"
        );
        assert_eq!(
            skipped,
            vec!["x86_64-pc-windows-msvc"],
            "windows-msvc still needs a Windows host, even from macOS"
        );
    }

    #[test]
    fn host_buildable_windows_host_keeps_msvc_skips_apple() {
        // A Windows host builds windows-msvc, but apple still needs macOS.
        let (kept, skipped) = host_buildable_targets(WINDOWS_HOST, &mixed_targets());
        assert_eq!(
            kept,
            vec![
                "x86_64-unknown-linux-gnu",
                "aarch64-unknown-linux-gnu",
                "x86_64-pc-windows-gnu",
                "x86_64-pc-windows-msvc",
            ],
            "windows host keeps windows-msvc + linux + windows-gnu: {kept:?}"
        );
        assert_eq!(
            skipped,
            vec!["x86_64-apple-darwin", "aarch64-apple-darwin"],
            "apple targets still need a macOS host, even from Windows"
        );
    }

    #[test]
    fn host_buildable_linux_only_config_keeps_all() {
        let configured = vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "x86_64-pc-windows-gnu".to_string(),
        ];
        let (kept, skipped) = host_buildable_targets(LINUX_HOST, &configured);
        assert_eq!(kept, configured);
        assert!(skipped.is_empty());
    }

    #[test]
    fn host_buildable_linux_apple_only_config_skips_all() {
        let configured = vec![
            "x86_64-apple-darwin".to_string(),
            "aarch64-apple-darwin".to_string(),
        ];
        let (kept, skipped) = host_buildable_targets(LINUX_HOST, &configured);
        assert!(kept.is_empty(), "a linux host can build no apple targets");
        assert_eq!(skipped, configured);
    }

    #[test]
    fn host_buildable_linux_msvc_only_config_skips_all() {
        let configured = vec!["x86_64-pc-windows-msvc".to_string()];
        let (kept, skipped) = host_buildable_targets(LINUX_HOST, &configured);
        assert!(
            kept.is_empty(),
            "a linux host can build no windows-msvc targets"
        );
        assert_eq!(skipped, configured);
    }

    #[test]
    fn host_targets_skip_message_names_both_reasons_on_linux() {
        // Mixed skip set on a linux host must group both reasons in one line.
        let skipped = vec![
            "aarch64-apple-darwin".to_string(),
            "x86_64-apple-darwin".to_string(),
            "x86_64-pc-windows-msvc".to_string(),
        ];
        let msg = host_targets_skip_message(LINUX_HOST, &skipped).unwrap();
        assert!(msg.contains("3 target(s)"), "names the count: {msg}");
        assert!(msg.contains("linux host"), "names the host OS: {msg}");
        assert!(
            msg.contains("apple targets require a macOS host"),
            "names the apple reason: {msg}"
        );
        assert!(
            msg.contains("windows-msvc targets require a Windows host"),
            "names the msvc reason: {msg}"
        );
        assert!(msg.contains("aarch64-apple-darwin"), "lists triple: {msg}");
        assert!(msg.contains("x86_64-apple-darwin"), "lists triple: {msg}");
        assert!(
            msg.contains("x86_64-pc-windows-msvc"),
            "lists triple: {msg}"
        );
        // Single grouped line — no per-target spam.
        assert_eq!(msg.lines().count(), 1, "stays a single line: {msg}");
    }

    #[test]
    fn host_targets_skip_message_msvc_only_omits_apple_clause() {
        // When only msvc is skipped, the message must NOT mention macOS.
        let skipped = vec!["x86_64-pc-windows-msvc".to_string()];
        let msg = host_targets_skip_message(LINUX_HOST, &skipped).unwrap();
        assert!(
            msg.contains("windows-msvc targets require a Windows host"),
            "names the msvc reason: {msg}"
        );
        assert!(
            !msg.contains("macOS"),
            "msvc-only skip must not mention macOS: {msg}"
        );
    }

    #[test]
    fn host_targets_skip_message_is_none_when_nothing_skipped() {
        assert!(host_targets_skip_message(LINUX_HOST, &[]).is_none());
    }

    #[test]
    fn parse_rustc_version_from_output_parses_release_line() {
        let sample = "\
rustc 1.96.0 (ac68faa20 2026-05-25)\n\
binary: rustc\n\
commit-hash: ac68faa20c58cbccd01ee7208bf3b6e93a7d7f96\n\
commit-date: 2026-05-25\n\
host: x86_64-unknown-linux-gnu\n\
release: 1.96.0\n\
LLVM version: 22.1.2\n";
        assert_eq!(
            parse_rustc_version_from_output(sample),
            Some("1.96.0".to_string())
        );
        // The same block must yield the host triple via the sibling parser.
        assert_eq!(
            parse_host_from_output(sample),
            Some("x86_64-unknown-linux-gnu".to_string())
        );
    }

    #[test]
    fn parse_rustc_version_from_output_parses_prerelease_line() {
        let sample = "\
rustc 1.97.0-nightly (abc123 2026-06-01)\n\
release: 1.97.0-nightly\n\
host: aarch64-apple-darwin\n";
        assert_eq!(
            parse_rustc_version_from_output(sample),
            Some("1.97.0-nightly".to_string())
        );
    }

    #[test]
    fn parse_rustc_version_from_output_returns_none_when_line_absent() {
        let sample = "binary: rustc\nhost: x86_64-unknown-linux-gnu\n";
        assert_eq!(parse_rustc_version_from_output(sample), None);
    }

    #[test]
    fn detect_rustc_version_live_returns_nonempty() {
        // Requires rustc on PATH — skip gracefully if absent.
        if let Some(ver) = detect_rustc_version() {
            assert!(!ver.is_empty(), "live rustc version should not be empty");
            assert!(
                ver.chars().next().is_some_and(|c| c.is_ascii_digit()),
                "live rustc version should start with a digit: {ver}"
            );
        }
    }
}
