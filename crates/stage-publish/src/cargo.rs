use anodizer_core::config::{CargoPublishConfig, CrateConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::util::topological_sort;
use anyhow::{Context as _, Result};
use std::collections::{HashMap, HashSet};
use std::process::Command;

/// Default seconds to wait for a freshly-published crate to appear in the
/// crates.io sparse index. Mirrors the historical anodizer default; only
/// matters when the crate has dependents that need it published first.
const DEFAULT_INDEX_TIMEOUT_SECS: u64 = 300;

/// Walk `depends_on` from each crate in `seed` to produce a de-duplicated
/// list containing every seed crate plus every transitive dependency that
/// lives in the same config. The `all_crates` slice is searched by name;
/// deps pointing at crates outside the config are ignored (same as cargo's
/// external-dep handling — they're expected to be on crates.io already).
fn expand_with_transitive_deps(all_crates: &[CrateConfig], seed: &[String]) -> Vec<String> {
    let name_to_deps: HashMap<&str, &[String]> = all_crates
        .iter()
        .map(|c| (c.name.as_str(), c.depends_on.as_deref().unwrap_or_default()))
        .collect();

    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = seed.to_vec();
    while let Some(name) = stack.pop() {
        // Skip names we've already visited or that aren't in the config —
        // external crates.io deps are resolved by cargo against the real
        // registry and don't need to appear in our publish graph.
        if !name_to_deps.contains_key(name.as_str()) {
            continue;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        out.push(name.clone());
        if let Some(deps) = name_to_deps.get(name.as_str()) {
            for dep in *deps {
                if !seen.contains(dep) {
                    stack.push(dep.clone());
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// publish_command
// ---------------------------------------------------------------------------

/// Build the argument list for `cargo publish` with the given config flags.
///
/// `--allow-dirty` is implicit: the pipeline runs after the tag step, which
/// ALWAYS leaves a dirty tree (Cargo.lock + version bump), so requiring a
/// clean tree would block every release. Users can still set
/// `cargo.allow_dirty: false` to opt out, but that's surprising enough we
/// always force-on by default.
pub fn publish_command(crate_name: &str, cfg: Option<&CargoPublishConfig>) -> Vec<String> {
    let mut cmd = vec![
        "cargo".to_string(),
        "publish".to_string(),
        "-p".to_string(),
        crate_name.to_string(),
    ];

    let Some(c) = cfg else {
        // No config block — preserve historical default of allow-dirty.
        cmd.push("--allow-dirty".to_string());
        return cmd;
    };

    // Registry selection
    if let Some(ref reg) = c.registry {
        cmd.push("--registry".to_string());
        cmd.push(reg.clone());
    }
    if let Some(ref idx) = c.index {
        cmd.push("--index".to_string());
        cmd.push(idx.clone());
    }

    // Verify / dirty
    if c.no_verify == Some(true) {
        cmd.push("--no-verify".to_string());
    }
    // allow_dirty defaults to ON when unset (anodize tag bumps Cargo.toml +
    // updates Cargo.lock, so the tree is always dirty by the time publish
    // runs). Setting `allow_dirty: false` explicitly disables it.
    if c.allow_dirty != Some(false) {
        cmd.push("--allow-dirty".to_string());
    }

    // Feature selection
    if let Some(ref feats) = c.features
        && !feats.is_empty()
    {
        cmd.push("--features".to_string());
        cmd.push(feats.join(","));
    }
    if c.all_features == Some(true) {
        cmd.push("--all-features".to_string());
    }
    if c.no_default_features == Some(true) {
        cmd.push("--no-default-features".to_string());
    }

    // Compilation
    if let Some(ref t) = c.target {
        cmd.push("--target".to_string());
        cmd.push(t.clone());
    }
    if let Some(ref td) = c.target_dir {
        cmd.push("--target-dir".to_string());
        cmd.push(td.display().to_string());
    }
    if let Some(j) = c.jobs {
        cmd.push("--jobs".to_string());
        cmd.push(j.to_string());
    }
    if c.keep_going == Some(true) {
        cmd.push("--keep-going".to_string());
    }

    // Manifest
    if let Some(ref mp) = c.manifest_path {
        cmd.push("--manifest-path".to_string());
        cmd.push(mp.display().to_string());
    }
    if c.locked == Some(true) {
        cmd.push("--locked".to_string());
    }
    if c.offline == Some(true) {
        cmd.push("--offline".to_string());
    }
    if c.frozen == Some(true) {
        cmd.push("--frozen".to_string());
    }

    cmd
}

// ---------------------------------------------------------------------------
// poll_crates_io_index
// ---------------------------------------------------------------------------

/// Build the sparse index URL for a crate name (path segments based on length).
///
/// Crate names per cargo are restricted to ASCII alphanumerics plus `-`/`_`
/// (cargo reference: "Crate names ... must be ASCII"), so the byte slices
/// below are guaranteed to land on character boundaries. The debug_assert
/// makes the invariant load-bearing — any caller passing a non-ASCII name
/// would surface the violation in a debug build long before the slice
/// could panic at runtime.
fn sparse_index_url(crate_name: &str) -> String {
    debug_assert!(
        crate_name.is_ascii(),
        "cargo crate names must be ASCII; got {crate_name:?}"
    );
    let lower = crate_name.to_ascii_lowercase();
    match lower.len() {
        1 => format!("https://index.crates.io/1/{}", lower),
        2 => format!("https://index.crates.io/2/{}", lower),
        3 => format!("https://index.crates.io/3/{}/{}", &lower[..1], lower),
        _ => format!(
            "https://index.crates.io/{}/{}/{}",
            &lower[..2],
            &lower[2..4],
            lower
        ),
    }
}

/// Check whether `crate_name` at `version` is already published on crates.io,
/// and if so, return the index-recorded sha256 cksum so callers can detect
/// drift between the local .crate and what's already on the registry.
///
/// Returns `Ok(Some(cksum_hex))` if the index has this version (cksum may be
/// an empty string if the index entry is malformed), `Ok(None)` if the crate
/// or version isn't present, `Err` on transport errors. Used to make publishes
/// idempotent across retries while surfacing same-version drift instead of
/// silently skipping a re-release that would install stale content.
fn is_already_published(crate_name: &str, version: &str) -> Result<Option<String>> {
    use std::time::Duration;

    let url = sparse_index_url(crate_name);
    let client = anodizer_core::http::blocking_client(Duration::from_secs(10))
        .context("publish: build HTTP client for index check")?;

    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("publish: query index for '{}'", crate_name))?;

    // 404 = crate has never been published — not already published.
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        anyhow::bail!(
            "publish: crates.io index returned {} for '{}'",
            resp.status(),
            crate_name
        );
    }

    let body = anodizer_core::http::body_of_blocking(resp);
    Ok(parse_index_cksum_for_version(&body, version))
}

/// Parse the crates.io sparse-index body (JSON-lines, one entry per
/// published version) and return the `cksum` for `version` when present.
///
/// - Returns `None` when no line matches the requested version.
/// - Returns `Some("")` when the version exists but the line is missing its
///   `cksum` field — caller must treat this as "version present, drift
///   undetectable" rather than "not published".
///
/// Extracted from `is_already_published` so the JSONL shape can be unit
/// tested without performing a network call to crates.io.
fn parse_index_cksum_for_version(body: &str, version: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let v = serde_json::from_str::<serde_json::Value>(line).ok()?;
        if v.get("vers")?.as_str()? != version {
            return None;
        }
        Some(
            v.get("cksum")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
        )
    })
}

/// Package the crate locally (`cargo package -p <name>`) and return the
/// sha256 hex cksum of the resulting .crate file — the same digest crates.io
/// records in the sparse index `cksum` field. Used to detect drift when a
/// version is already published.
fn compute_local_crate_cksum(crate_name: &str, version: &str) -> Result<String> {
    use sha2::Digest as _;

    // Ensure the .crate file exists locally. `cargo package` is idempotent
    // when inputs haven't changed, so this is cheap on warm builds.
    let output = Command::new("cargo")
        .args([
            "package",
            "-p",
            crate_name,
            "--allow-dirty",
            "--no-verify",
            "--quiet",
        ])
        .output()
        .with_context(|| format!("publish: spawn `cargo package -p {}`", crate_name))?;
    if !output.status.success() {
        anyhow::bail!(
            "publish: `cargo package -p {}` failed: {}",
            crate_name,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    let crate_path = std::path::PathBuf::from(&target_dir)
        .join("package")
        .join(format!("{}-{}.crate", crate_name, version));
    let bytes = std::fs::read(&crate_path)
        .with_context(|| format!("publish: read packaged crate {}", crate_path.display()))?;
    let digest = sha2::Sha256::digest(&bytes);
    Ok(digest.iter().map(|b| format!("{:02x}", b)).collect())
}

/// Poll the crates.io sparse index until `crate_name` at `version` appears or
/// the deadline (seconds) is exceeded.  Uses exponential back-off starting at
/// `INITIAL_POLL_DELAY`, capped at `MAX_POLL_DELAY`.
///
/// Returns `Ok(())` when the version is confirmed, `Err` on timeout.
fn poll_crates_io_index(
    crate_name: &str,
    version: &str,
    timeout_secs: u64,
    log: &StageLogger,
) -> Result<()> {
    use std::time::{Duration, Instant};

    const INITIAL_POLL_DELAY: Duration = Duration::from_secs(5);
    const MAX_POLL_DELAY: Duration = Duration::from_secs(60);

    let start = Instant::now();
    let deadline = Duration::from_secs(timeout_secs);
    let url = sparse_index_url(crate_name);

    let client = anodizer_core::http::blocking_client(Duration::from_secs(10))
        .context("publish: build HTTP client for index polling")?;

    let mut backoff = INITIAL_POLL_DELAY;

    loop {
        match client.get(&url).send() {
            Ok(resp) if resp.status().is_success() => {
                let body = anodizer_core::http::body_of_blocking(resp);
                // Each line of the sparse index is a JSON object; parse and check vers field.
                if body.lines().any(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .ok()
                        .and_then(|v| v.get("vers")?.as_str().map(|s| s == version))
                        .unwrap_or(false)
                }) {
                    log.status(&format!(
                        "crates.io index confirmed {}-{}",
                        crate_name, version
                    ));
                    return Ok(());
                }
            }
            Ok(resp) => {
                log.warn(&format!(
                    "crates.io index returned {} for {}, retrying…",
                    resp.status(),
                    crate_name
                ));
            }
            Err(e) => {
                log.error(&format!(
                    "HTTP error polling index for {}: {}",
                    crate_name, e
                ));
            }
        }

        if start.elapsed() >= deadline {
            anyhow::bail!(
                "publish: timed out waiting for {}-{} to appear in crates.io index \
                 (waited {} s)",
                crate_name,
                version,
                timeout_secs
            );
        }

        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(MAX_POLL_DELAY);
    }
}

// ---------------------------------------------------------------------------
// publish_to_cargo
// ---------------------------------------------------------------------------

/// Read the `version = "X.Y.Z"` from a crate's Cargo.toml.
/// Uses a simple line scan rather than a full TOML parse to avoid
/// pulling in the `toml` crate as a dep of stage-publish.
fn read_cargo_toml_version(crate_path: &str) -> Option<String> {
    let manifest = std::path::Path::new(crate_path).join("Cargo.toml");
    let content = std::fs::read_to_string(&manifest).ok()?;
    // Look for `version = "..."` in the [package] section (before any
    // other `[section]` header). This covers both quoted and workspace
    // forms; workspace references (version.workspace = true) return None
    // since they don't have a literal version string.
    let mut in_package = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[package]" {
            in_package = true;
            continue;
        }
        if trimmed.starts_with('[') {
            if in_package {
                break;
            }
            continue;
        }
        if in_package && let Some(rest) = trimmed.strip_prefix("version") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim();
                if rest.starts_with('"') {
                    return rest.trim_matches('"').to_string().into();
                }
            }
        }
    }
    None
}

pub fn publish_to_cargo(ctx: &mut Context, selected: &[String], log: &StageLogger) -> Result<()> {
    // Defensive guard: the `--skip=cargo` gate (FOLL-1) lives in the
    // dispatcher in `lib.rs::PublishStage::run` so every publisher emits its
    // skip log uniformly. Re-checking here protects future direct callers
    // (tests, CLI sub-commands) from accidentally bypassing the gate. No log
    // is emitted on this path — the dispatcher already logged it.
    if ctx.should_skip("cargo") {
        return Ok(());
    }
    // When a crate depends on another crate in the same workspace that
    // isn't yet on crates.io, `cargo publish` for the dependent will fail
    // with "no matching package named X found" because cargo verifies path
    // deps against the registry. Walk depends_on transitively so we publish
    // the dependency chain in topological order, not just the caller's
    // --crate selection. Already-published versions are skipped below via
    // the is_already_published check, so including extra crates is safe.
    // Build the full crate universe — top-level + all workspaces — so
    // expand_with_transitive_deps can find deps that live in a DIFFERENT
    // workspace. After workspace overlay, config.crates only contains the
    // overlaid workspace's crates, but a crate's depends_on may reference
    // crates in other workspaces (e.g. cfgd depends on cfgd-core which
    // lives in its own workspace).
    let all_crates: Vec<CrateConfig> = crate::util::all_crates(ctx);

    let expanded_selection: Vec<String> = if selected.is_empty() {
        Vec::new()
    } else {
        expand_with_transitive_deps(&all_crates, selected)
    };
    let selected_set: std::collections::HashSet<&str> =
        expanded_selection.iter().map(|s| s.as_str()).collect();

    // Resolve the per-crate `publish.cargo` block to a (selected, cfg) pair.
    // - None       → publisher omitted; not eligible.
    // - Some(cfg)  → eligible unless `cfg.skip` evaluates truthy.
    // Templated `skip:` is honored here so the same render-once pass populates
    // both the eligibility list and the per-crate timeout/flag lookups.
    let cargo_cfgs: HashMap<String, CargoPublishConfig> = {
        let mut m = HashMap::new();
        for c in &all_crates {
            let Some(ref publish) = c.publish else {
                continue;
            };
            let Some(ref cargo_cfg) = publish.cargo else {
                continue;
            };
            // Honor the peer-publisher `skip:` field (DEC-6).
            if let Some(ref d) = cargo_cfg.skip {
                let off = d
                    .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .with_context(|| format!("cargo: render skip template for '{}'", c.name))?;
                if off {
                    log.status(&format!("cargo: skipped for '{}' (skip=true)", c.name));
                    continue;
                }
            }
            m.insert(c.name.clone(), cargo_cfg.clone());
        }
        m
    };

    // Collect (name, depends_on) for crates with cargo publishing eligible,
    // filtered to the expanded selection when non-empty.
    let publishable: Vec<(String, Vec<String>)> = all_crates
        .iter()
        .filter(|c| selected.is_empty() || selected_set.contains(c.name.as_str()))
        .filter(|c| cargo_cfgs.contains_key(&c.name))
        .map(|c| {
            let deps = c.depends_on.clone().unwrap_or_default();
            (c.name.clone(), deps)
        })
        .collect();

    if publishable.is_empty() {
        log.status("no crates configured for crates.io publishing");
        return Ok(());
    }

    let sorted_names = topological_sort(&publishable);

    // Build a quick lookup: name → depends_on
    let deps_map: HashMap<String, Vec<String>> = all_crates
        .iter()
        .map(|c| (c.name.clone(), c.depends_on.clone().unwrap_or_default()))
        .collect();

    if ctx.is_dry_run() {
        for name in &sorted_names {
            let cmd = publish_command(name, cargo_cfgs.get(name));
            log.status(&format!("(dry-run) would run: {}", cmd.join(" ")));
        }
        return Ok(());
    }

    let version = ctx.version();

    // Build a lookup from crate name → path so we can read each crate's
    // actual Cargo.toml version for the already-published check. Transitive
    // deps may have a DIFFERENT version than the release tag (e.g. cfgd-core
    // is at 0.2.2 while cfgd releases 0.3.2).
    let crate_paths: HashMap<String, String> = all_crates
        .iter()
        .map(|c| (c.name.clone(), c.path.clone()))
        .collect();

    for (i, name) in sorted_names.iter().enumerate() {
        // Read the crate's actual version from its Cargo.toml, falling back
        // to the global release version if the path isn't found or parse fails.
        let crate_version = crate_paths
            .get(name)
            .and_then(|path| read_cargo_toml_version(path))
            .unwrap_or_else(|| version.clone());

        // Idempotency with drift detection: if this version is already on
        // crates.io, only skip when the local .crate matches the index cksum.
        // crates.io versions are immutable once published — if the local
        // bytes differ (typically because the same tag was re-cut against a
        // different commit), the cached crates.io content is stale and
        // silently skipping would leave users on `cargo install` getting
        // content that doesn't match the git tag. Bail with explicit "bump
        // version" guidance instead.
        //
        // Index check failures are non-fatal — we still try to publish and
        // let cargo's server-side guard (409 Conflict) catch real drift.
        let published_cksum = if crate_version.is_empty() {
            None
        } else {
            match is_already_published(name, &crate_version) {
                Ok(c) => c,
                Err(e) => {
                    log.warn(&format!(
                        "could not check crates.io index for '{}-{}' ({}); attempting publish anyway",
                        name, crate_version, e
                    ));
                    None
                }
            }
        };
        if let Some(index_cksum) = published_cksum {
            if index_cksum.is_empty() {
                // Index entry exists but has no cksum we can read. Fall back
                // to the historical skip behaviour rather than error, since
                // we can't verify drift.
                log.status(&format!(
                    "skipping '{}-{}' — already published on crates.io (index cksum unavailable, not verifying)",
                    name, crate_version
                ));
                continue;
            }
            match compute_local_crate_cksum(name, &crate_version) {
                Ok(local_cksum) if local_cksum == index_cksum => {
                    log.status(&format!(
                        "skipping '{}-{}' — already published on crates.io (cksum match)",
                        name, crate_version
                    ));
                    continue;
                }
                Ok(local_cksum) => {
                    anyhow::bail!(
                        "crates.io: '{}-{}' is already published but the local .crate differs \
                         (index sha256={}, local sha256={}). crates.io versions are immutable \
                         once published — bump the version before re-releasing.",
                        name,
                        crate_version,
                        index_cksum,
                        local_cksum
                    );
                }
                Err(e) => {
                    log.warn(&format!(
                        "could not compute local .crate cksum for '{}-{}' ({}); \
                         skipping re-publish of the already-published version",
                        name, crate_version, e
                    ));
                    continue;
                }
            }
        }

        let cargo_cfg = cargo_cfgs.get(name);
        let cmd = publish_command(name, cargo_cfg);
        log.status(&format!("running: {}", cmd.join(" ")));

        let output = Command::new(&cmd[0])
            .args(&cmd[1..])
            .output()
            .with_context(|| format!("publish: spawn `{}`", cmd.join(" ")))?;

        log.check_output(output, &format!("cargo publish -p {}", name))?;

        log.status(&format!("published crate '{}'", name));

        // If there are later crates that depend on this one, wait for the index.
        let has_dependents = sorted_names[i + 1..].iter().any(|later| {
            deps_map
                .get(later)
                .map(|d| d.contains(name))
                .unwrap_or(false)
        });

        if has_dependents && !crate_version.is_empty() {
            let timeout = cargo_cfg
                .and_then(|c| c.index_timeout)
                .unwrap_or(DEFAULT_INDEX_TIMEOUT_SECS);
            if timeout == 0 {
                log.warn(&format!(
                    "index_timeout is 0 for '{}'; skipping index poll (dependents may fail)",
                    name
                ));
            } else {
                log.status(&format!(
                    "waiting for {}-{} in crates.io index (timeout={}s)…",
                    name, crate_version, timeout
                ));
                poll_crates_io_index(name, &crate_version, timeout, log)
                    .with_context(|| format!("publish: index poll for '{}'", name))?;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topo_sort_simple() {
        let order = vec![
            ("cfgd-core".to_string(), vec![]),
            ("cfgd".to_string(), vec!["cfgd-core".to_string()]),
        ];
        let sorted = topological_sort(&order);
        assert_eq!(sorted, vec!["cfgd-core", "cfgd"]);
    }

    #[test]
    fn test_topo_sort_no_deps() {
        let order = vec![("a".to_string(), vec![]), ("b".to_string(), vec![])];
        let sorted = topological_sort(&order);
        assert_eq!(sorted.len(), 2);
    }

    #[test]
    fn test_publish_command_default() {
        // No config block — historical behaviour preserved (--allow-dirty on).
        let cmd = publish_command("my-crate", None);
        assert_eq!(
            cmd,
            vec![
                "cargo".to_string(),
                "publish".to_string(),
                "-p".to_string(),
                "my-crate".to_string(),
                "--allow-dirty".to_string(),
            ]
        );
    }

    #[test]
    fn test_publish_command_full_flag_surface() {
        let cfg = CargoPublishConfig {
            registry: Some("alt-registry".to_string()),
            index: Some("https://example.com/idx".to_string()),
            no_verify: Some(true),
            allow_dirty: Some(true),
            features: Some(vec!["a".to_string(), "b".to_string()]),
            all_features: Some(true),
            no_default_features: Some(true),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            target_dir: Some(std::path::PathBuf::from("/tmp/td")),
            jobs: Some(4),
            keep_going: Some(true),
            manifest_path: Some(std::path::PathBuf::from("./Cargo.toml")),
            locked: Some(true),
            offline: Some(true),
            frozen: Some(true),
            ..Default::default()
        };
        let cmd = publish_command("my-crate", Some(&cfg));

        // Helper: assert the flag is present and (for value-bearing flags)
        // the immediately-next argv slot holds the expected value. Catches
        // bugs where two adjacent flag/value pairs swap.
        let assert_value = |flag: &str, expected: &str| {
            let pos = cmd
                .iter()
                .position(|s| s == flag)
                .unwrap_or_else(|| panic!("missing flag {flag}: {cmd:?}"));
            assert_eq!(
                cmd[pos + 1],
                expected,
                "{flag} value mismatch (full cmd: {cmd:?})"
            );
        };
        let assert_present = |flag: &str| {
            assert!(
                cmd.iter().any(|s| s == flag),
                "missing flag {flag}: {cmd:?}"
            );
        };

        // Value-bearing flags — assert flag + adjacent value at pos+1.
        assert_value("--registry", "alt-registry");
        assert_value("--index", "https://example.com/idx");
        assert_value("--features", "a,b"); // features are comma-joined
        assert_value("--target", "x86_64-unknown-linux-gnu");
        assert_value("--target-dir", "/tmp/td");
        assert_value("--jobs", "4");
        assert_value("--manifest-path", "./Cargo.toml");

        // Boolean flags — only need to assert presence (no following value).
        for flag in [
            "--no-verify",
            "--allow-dirty",
            "--all-features",
            "--no-default-features",
            "--keep-going",
            "--locked",
            "--offline",
            "--frozen",
        ] {
            assert_present(flag);
        }
    }

    #[test]
    fn test_publish_command_allow_dirty_explicit_false() {
        let cfg = CargoPublishConfig {
            allow_dirty: Some(false),
            ..Default::default()
        };
        let cmd = publish_command("my-crate", Some(&cfg));
        assert!(
            !cmd.iter().any(|s| s == "--allow-dirty"),
            "explicit allow_dirty=false should suppress the flag: {cmd:?}"
        );
    }

    fn crate_with_deps(name: &str, deps: &[&str]) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn test_expand_transitive_deps_includes_direct_dep() {
        // --crate cfgd should expand to [cfgd, cfgd-core] so cfgd-core
        // gets published before cfgd tries to reference it on crates.io.
        let crates = vec![
            crate_with_deps("cfgd-core", &[]),
            crate_with_deps("cfgd", &["cfgd-core"]),
        ];
        let selection = vec!["cfgd".to_string()];
        let expanded = expand_with_transitive_deps(&crates, &selection);
        assert!(expanded.contains(&"cfgd".to_string()));
        assert!(expanded.contains(&"cfgd-core".to_string()));
        assert_eq!(expanded.len(), 2);
    }

    #[test]
    fn test_expand_transitive_deps_chains_through_multiple_levels() {
        let crates = vec![
            crate_with_deps("a", &[]),
            crate_with_deps("b", &["a"]),
            crate_with_deps("c", &["b"]),
        ];
        let expanded = expand_with_transitive_deps(&crates, &["c".to_string()]);
        assert!(expanded.contains(&"a".to_string()));
        assert!(expanded.contains(&"b".to_string()));
        assert!(expanded.contains(&"c".to_string()));
    }

    #[test]
    fn test_expand_transitive_deps_dedupes_shared_ancestors() {
        // diamond: d depends on both b and c, which both depend on a.
        let crates = vec![
            crate_with_deps("a", &[]),
            crate_with_deps("b", &["a"]),
            crate_with_deps("c", &["a"]),
            crate_with_deps("d", &["b", "c"]),
        ];
        let expanded = expand_with_transitive_deps(&crates, &["d".to_string()]);
        assert_eq!(
            expanded.len(),
            4,
            "expected all 4 crates once: {:?}",
            expanded
        );
    }

    #[test]
    fn test_expand_transitive_deps_ignores_external_deps() {
        // Deps on names not present in the config (i.e. external crates.io
        // crates) are silently dropped — cargo verifies them against the
        // real registry, not our workspace.
        let crates = vec![crate_with_deps("cfgd", &["cfgd-core", "serde"])];
        let expanded = expand_with_transitive_deps(&crates, &["cfgd".to_string()]);
        assert!(expanded.contains(&"cfgd".to_string()));
        // cfgd-core isn't in the config, so it won't appear
        assert!(!expanded.contains(&"cfgd-core".to_string()));
        assert!(!expanded.contains(&"serde".to_string()));
    }

    // -----------------------------------------------------------------------
    // crates.io idempotency (C-new-11 / C-new-13)
    //
    // The hash-match short-circuit in publish_to_cargo (cf. cargo.rs
    // ~line 489) avoids redundant `cargo publish` calls — and the bogus
    // 422-with-stale-bytes problem they create — when the version already
    // exists on crates.io and the local .crate cksum matches the index. The
    // tests below pin (a) the sparse-index URL shape so we hit the same
    // path cargo itself uses, and (b) the JSONL parser so we keep treating
    // "version present, no cksum" as a fall-back-to-skip rather than a
    // silently-missed publish.
    // -----------------------------------------------------------------------

    /// Sparse-index URL must follow the cargo registry layout:
    /// 1-char names live under `/1/<name>`, 2-char under `/2/<name>`,
    /// 3-char under `/3/<first>/<name>`, 4+ under `/<first2>/<next2>/<name>`.
    /// Mismatch here means we'd query a URL that always 404s and silently
    /// re-publish every release.
    #[test]
    fn test_sparse_index_url_shape() {
        // 1-char crate name.
        assert_eq!(sparse_index_url("a"), "https://index.crates.io/1/a");
        // 2-char.
        assert_eq!(sparse_index_url("ab"), "https://index.crates.io/2/ab");
        // 3-char — `/3/<first>/<name>`.
        assert_eq!(sparse_index_url("abc"), "https://index.crates.io/3/a/abc");
        // 4-char — `/<first2>/<next2>/<name>`.
        assert_eq!(
            sparse_index_url("abcd"),
            "https://index.crates.io/ab/cd/abcd"
        );
        // Real-world case (5+ char): `cfgd-core`.
        assert_eq!(
            sparse_index_url("cfgd-core"),
            "https://index.crates.io/cf/gd/cfgd-core"
        );
        // Uppercase normalises to lowercase per cargo registry spec.
        assert_eq!(
            sparse_index_url("MyTool"),
            "https://index.crates.io/my/to/mytool"
        );
    }

    /// Parser returns the cksum only when a line matches the requested
    /// version; mismatched-version lines and absent fields short-circuit
    /// to None/empty respectively.
    #[test]
    fn test_parse_index_cksum_for_version_matches_requested_version() {
        // Two versions on the index; only 1.2.3's cksum should come back.
        let body = r#"{"name":"foo","vers":"1.2.2","cksum":"old","yanked":false}
{"name":"foo","vers":"1.2.3","cksum":"newhash","yanked":false}
{"name":"foo","vers":"1.2.4","cksum":"newer","yanked":false}"#;
        assert_eq!(
            parse_index_cksum_for_version(body, "1.2.3"),
            Some("newhash".to_string())
        );
    }

    #[test]
    fn test_parse_index_cksum_for_version_returns_none_when_absent() {
        // Index has 1.2.2 but caller asked for 1.2.3 — must return None so
        // publish_to_cargo proceeds with the publish.
        let body = r#"{"name":"foo","vers":"1.2.2","cksum":"old","yanked":false}"#;
        assert_eq!(parse_index_cksum_for_version(body, "1.2.3"), None);
    }

    #[test]
    fn test_parse_index_cksum_for_version_empty_string_when_cksum_missing() {
        // Index entry has the requested version but no `cksum` field
        // (malformed/legacy entry). Returning Some("") signals "present but
        // drift undetectable" so the caller falls back to the historical
        // skip behaviour rather than mis-treating it as "not published".
        let body = r#"{"name":"foo","vers":"1.2.3","yanked":false}"#;
        assert_eq!(
            parse_index_cksum_for_version(body, "1.2.3"),
            Some(String::new())
        );
    }

    #[test]
    fn test_parse_index_cksum_for_version_empty_body() {
        // Defensive: an empty/whitespace body parses to None (the function
        // is invoked after a 200-OK status but before further validation,
        // so we mustn't panic on malformed bodies).
        assert_eq!(parse_index_cksum_for_version("", "1.0.0"), None);
        assert_eq!(parse_index_cksum_for_version("   \n  ", "1.0.0"), None);
    }

    #[test]
    fn test_parse_index_cksum_for_version_skips_garbage_lines() {
        // A non-JSON line in the middle must not abort the scan — cargo's
        // own client tolerates trailing newlines and similar.
        let body = "not-json\n{\"name\":\"foo\",\"vers\":\"1.2.3\",\"cksum\":\"abcd\"}\n";
        assert_eq!(
            parse_index_cksum_for_version(body, "1.2.3"),
            Some("abcd".to_string())
        );
    }
}
