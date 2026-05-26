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
        .unwrap_or("goos");

    match by {
        "goos" => {
            let (os, _) = target::map_target(&host);
            Ok(PartialTarget::OsArch { os, arch: None })
        }
        "target" => Ok(PartialTarget::Exact(host)),
        other => anyhow::bail!(
            "partial.by: unknown value '{}' (expected 'goos' or 'target')",
            other
        ),
    }
}

/// Detect the host target triple via `rustc -vV`.
pub fn detect_host_target() -> Result<String> {
    let mut cmd = std::process::Command::new("rustc");
    cmd.args(["-vV"]);
    tracing::debug!(args = ?cmd.get_args(), "spawning rustc for host target detection");
    let output = cmd
        .output()
        .context("failed to run `rustc -vV` for host target detection")?;

    if !output.status.success() {
        anyhow::bail!(
            "rustc -vV failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(host) = line.strip_prefix("host: ") {
            return Ok(host.trim().to_string());
        }
    }
    anyhow::bail!("could not detect host target from `rustc -vV` output")
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
    fn test_resolve_with_goos_default() {
        // Clear env vars that might interfere
        // SAFETY: test-only, no concurrent env var access in these serial tests
        unsafe {
            std::env::remove_var("TARGET");
            std::env::remove_var("ANODIZER_OS");
            std::env::remove_var("ANODIZER_ARCH");
        }

        let config = None; // defaults to "goos"
        let target = resolve_partial_target(&config).unwrap();

        // Should be an OsArch with the host's OS
        match target {
            PartialTarget::OsArch { os, arch } => {
                assert!(!os.is_empty());
                assert!(arch.is_none()); // goos mode doesn't set arch
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
}
