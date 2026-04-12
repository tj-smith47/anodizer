use anodize_core::config::CrateConfig;
use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anodize_core::util::topological_sort;
use anyhow::{Context as _, Result};
use std::collections::{HashMap, HashSet};
use std::process::Command;

/// Walk `depends_on` from each crate in `seed` to produce a de-duplicated
/// list containing every seed crate plus every transitive dependency that
/// lives in the same config. The `all_crates` slice is searched by name;
/// deps pointing at crates outside the config are ignored (same as cargo's
/// external-dep handling — they're expected to be on crates.io already).
fn expand_with_transitive_deps(all_crates: &[CrateConfig], seed: &[String]) -> Vec<String> {
    let name_to_deps: HashMap<&str, &[String]> = all_crates
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.depends_on
                    .as_deref()
                    .unwrap_or_default(),
            )
        })
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

/// Build the argument list for `cargo publish -p <crate_name> --allow-dirty`.
pub fn publish_command(crate_name: &str) -> Vec<String> {
    vec![
        "cargo".to_string(),
        "publish".to_string(),
        "-p".to_string(),
        crate_name.to_string(),
        "--allow-dirty".to_string(),
    ]
}

// ---------------------------------------------------------------------------
// poll_crates_io_index
// ---------------------------------------------------------------------------

/// Build the sparse index URL for a crate name (path segments based on length).
fn sparse_index_url(crate_name: &str) -> String {
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

/// Check whether `crate_name` at `version` is already published on crates.io.
///
/// Returns `Ok(true)` if the index has the version, `Ok(false)` if the crate
/// doesn't exist or the specific version isn't there, `Err` on transport errors.
/// Used to make publishes idempotent across retries.
fn is_already_published(crate_name: &str, version: &str) -> Result<bool> {
    use std::time::Duration;

    let url = sparse_index_url(crate_name);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("publish: build HTTP client for index check")?;

    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("publish: query index for '{}'", crate_name))?;

    // 404 = crate has never been published — not already published.
    if resp.status().as_u16() == 404 {
        return Ok(false);
    }
    if !resp.status().is_success() {
        anyhow::bail!(
            "publish: crates.io index returned {} for '{}'",
            resp.status(),
            crate_name
        );
    }

    let body = resp.text().unwrap_or_default();
    let found = body.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v.get("vers")?.as_str().map(|s| s == version))
            .unwrap_or(false)
    });
    Ok(found)
}

/// Poll the crates.io sparse index until `crate_name` at `version` appears or
/// the deadline (seconds) is exceeded.  Uses exponential back-off starting at
/// 5 s, capped at 60 s.
///
/// Returns `Ok(())` when the version is confirmed, `Err` on timeout.
fn poll_crates_io_index(
    crate_name: &str,
    version: &str,
    timeout_secs: u64,
    log: &StageLogger,
) -> Result<()> {
    use std::time::{Duration, Instant};

    let start = Instant::now();
    let deadline = Duration::from_secs(timeout_secs);
    let url = sparse_index_url(crate_name);

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("publish: build HTTP client for index polling")?;

    let mut backoff = Duration::from_secs(5);
    let cap = Duration::from_secs(60);

    loop {
        match client.get(&url).send() {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.text().unwrap_or_default();
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
        backoff = (backoff * 2).min(cap);
    }
}

// ---------------------------------------------------------------------------
// publish_to_crates_io
// ---------------------------------------------------------------------------

pub fn publish_to_crates_io(
    ctx: &mut Context,
    selected: &[String],
    log: &StageLogger,
) -> Result<()> {
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
    let all_crates: Vec<CrateConfig> = {
        let mut acc = ctx.config.crates.clone();
        if let Some(ref ws_list) = ctx.config.workspaces {
            for ws in ws_list {
                for c in &ws.crates {
                    if !acc.iter().any(|existing| existing.name == c.name) {
                        acc.push(c.clone());
                    }
                }
            }
        }
        acc
    };

    let expanded_selection: Vec<String> = if selected.is_empty() {
        Vec::new()
    } else {
        expand_with_transitive_deps(&all_crates, selected)
    };
    let selected_set: std::collections::HashSet<&str> =
        expanded_selection.iter().map(|s| s.as_str()).collect();

    // Collect (name, depends_on) for all crates with crates.io publishing enabled,
    // filtered to the expanded selection when non-empty. Uses all_crates (the
    // flattened universe) so transitive deps from other workspaces are included.
    let publishable: Vec<(String, Vec<String>)> = all_crates
        .iter()
        .filter(|c| selected.is_empty() || selected_set.contains(c.name.as_str()))
        .filter(|c| {
            c.publish
                .as_ref()
                .map(|p| p.crates_config().enabled)
                .unwrap_or(false)
        })
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

    // Build a quick lookup: name → index_timeout
    let timeout_map: HashMap<String, u64> = all_crates
        .iter()
        .filter_map(|c| {
            c.publish
                .as_ref()
                .map(|p| (c.name.clone(), p.crates_config().index_timeout))
        })
        .collect();

    // Build a quick lookup: name → depends_on
    let deps_map: HashMap<String, Vec<String>> = all_crates
        .iter()
        .map(|c| (c.name.clone(), c.depends_on.clone().unwrap_or_default()))
        .collect();

    if ctx.is_dry_run() {
        for name in &sorted_names {
            let cmd = publish_command(name);
            log.status(&format!("(dry-run) would run: {}", cmd.join(" ")));
        }
        return Ok(());
    }

    let version = ctx.version();

    for (i, name) in sorted_names.iter().enumerate() {
        // Idempotency: skip if this version is already on crates.io.  Lets the
        // publish stage tolerate retries after a partial run (e.g. crates.io
        // new-crate rate-limit) without failing on the first already-published
        // crate.  Index check failures are non-fatal — we still try to publish.
        let already = if version.is_empty() {
            false
        } else {
            match is_already_published(name, &version) {
                Ok(found) => found,
                Err(e) => {
                    log.warn(&format!(
                        "could not check crates.io index for '{}-{}' ({}); attempting publish anyway",
                        name, version, e
                    ));
                    false
                }
            }
        };
        if already {
            log.status(&format!(
                "skipping '{}-{}' — already published on crates.io",
                name, version
            ));
            continue;
        }

        let cmd = publish_command(name);
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

        if has_dependents && !version.is_empty() {
            let timeout = timeout_map.get(name).copied().unwrap_or(300);
            if timeout == 0 {
                // index_timeout: 0 — skip the poll entirely and warn. Useful
                // when the caller is willing to accept a downstream publish
                // failure if the index hasn't propagated yet.
                log.warn(&format!(
                    "index_timeout is 0 for '{}'; skipping index poll (dependents may fail)",
                    name
                ));
            } else {
                log.status(&format!(
                    "waiting for {}-{} in crates.io index (timeout={}s)…",
                    name, version, timeout
                ));
                poll_crates_io_index(name, &version, timeout, log)
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
    fn test_publish_command() {
        let cmd = publish_command("my-crate");
        assert!(cmd.contains(&"publish".to_string()));
        assert!(cmd.contains(&"-p".to_string()));
        assert!(cmd.contains(&"my-crate".to_string()));
    }

    fn crate_with_deps(name: &str, deps: &[&str]) -> CrateConfig {
        let mut c = CrateConfig::default();
        c.name = name.to_string();
        c.depends_on = Some(deps.iter().map(|s| s.to_string()).collect());
        c
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
        assert_eq!(expanded.len(), 4, "expected all 4 crates once: {:?}", expanded);
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
}
