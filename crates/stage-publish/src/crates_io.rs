use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anodize_core::util::topological_sort;
use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::process::Command;

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

    let sorted_names = topological_sort(&publishable);

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

    let version = ctx.version();

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
}
