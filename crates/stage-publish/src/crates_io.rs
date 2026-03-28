use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Command;

// ---------------------------------------------------------------------------
// topo_sort
// ---------------------------------------------------------------------------

/// Topological sort of crates by their `depends_on` lists.
///
/// Input: slice of `(crate_name, depends_on_names)`.
/// Output: names in publish order (dependencies before dependents).
/// If a crate listed in `depends_on` is not in the input set it is ignored.
pub fn topo_sort(crates: &[(String, Vec<String>)]) -> Vec<String> {
    // Build adjacency (dependency → dependent) and in-degree maps.
    let names: HashSet<&str> = crates.iter().map(|(n, _)| n.as_str()).collect();

    // in_degree: how many unresolved deps each node has
    let mut in_degree: HashMap<&str, usize> = crates
        .iter()
        .map(|(n, deps)| {
            let deg = deps.iter().filter(|d| names.contains(d.as_str())).count();
            (n.as_str(), deg)
        })
        .collect();

    // edges: dep → list of nodes that depend on dep
    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for (n, deps) in crates {
        for dep in deps {
            if names.contains(dep.as_str()) {
                edges.entry(dep.as_str()).or_default().push(n.as_str());
            }
        }
    }

    // Kahn's algorithm — deterministic: sort the initial zero-in-degree queue
    let mut queue: VecDeque<&str> = {
        let mut v: Vec<&str> = in_degree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(&n, _)| n)
            .collect();
        v.sort_unstable();
        VecDeque::from(v)
    };

    let mut result = Vec::with_capacity(crates.len());
    while let Some(node) = queue.pop_front() {
        result.push(node.to_string());
        if let Some(dependents) = edges.get(node) {
            let mut next: Vec<&str> = dependents
                .iter()
                .filter_map(|&dep| {
                    let deg = in_degree.get_mut(dep)?;
                    *deg -= 1;
                    if *deg == 0 { Some(dep) } else { None }
                })
                .collect();
            next.sort_unstable();
            for n in next {
                queue.push_back(n);
            }
        }
    }

    result
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

    // Sparse index URL: lowercase first two chars form the path segments.
    let lower = crate_name.to_ascii_lowercase();
    let url = match lower.len() {
        1 => format!("https://index.crates.io/1/{}", lower),
        2 => format!("https://index.crates.io/2/{}", lower),
        3 => format!("https://index.crates.io/3/{}/{}", &lower[..1], lower),
        _ => format!(
            "https://index.crates.io/{}/{}/{}",
            &lower[..2],
            &lower[2..4],
            lower
        ),
    };

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
    // Collect (name, depends_on) for all crates with crates.io publishing enabled,
    // filtered to `selected` when non-empty.
    let publishable: Vec<(String, Vec<String>)> = ctx
        .config
        .crates
        .iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
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

    let sorted_names = topo_sort(&publishable);

    // Build a quick lookup: name → index_timeout
    let timeout_map: HashMap<String, u64> = ctx
        .config
        .crates
        .iter()
        .filter_map(|c| {
            c.publish
                .as_ref()
                .map(|p| (c.name.clone(), p.crates_config().index_timeout))
        })
        .collect();

    // Build a quick lookup: name → depends_on
    let deps_map: HashMap<String, Vec<String>> = ctx
        .config
        .crates
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

    // Resolve the version from template vars (best-effort).
    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_default();

    for (i, name) in sorted_names.iter().enumerate() {
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
            log.status(&format!(
                "waiting for {}-{} in crates.io index (timeout={}s)…",
                name, version, timeout
            ));
            poll_crates_io_index(name, &version, timeout, log)
                .with_context(|| format!("publish: index poll for '{}'", name))?;
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
        let sorted = topo_sort(&order);
        assert_eq!(sorted, vec!["cfgd-core", "cfgd"]);
    }

    #[test]
    fn test_topo_sort_no_deps() {
        let order = vec![("a".to_string(), vec![]), ("b".to_string(), vec![])];
        let sorted = topo_sort(&order);
        assert_eq!(sorted.len(), 2);
    }

    #[test]
    fn test_publish_command() {
        let cmd = publish_command("my-crate");
        assert!(cmd.contains(&"publish".to_string()));
        assert!(cmd.contains(&"-p".to_string()));
        assert!(cmd.contains(&"my-crate".to_string()));
    }
}
