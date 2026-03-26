use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use anodize_core::config::{Config, CrateConfig};
use crate::pipeline;

pub fn run(config_override: Option<&Path>) -> Result<()> {
    let path = pipeline::find_config(config_override)?;
    let config = pipeline::load_config(&path)?;
    run_checks(&config, true)
}

/// Core validation logic. `check_env` controls whether env/tool checks are run
/// (so tests can skip them).
pub fn run_checks(config: &Config, check_env: bool) -> Result<()> {
    let mut errors: Vec<String> = vec![];
    let mut warnings: Vec<String> = vec![];

    // ------------------------------------------------------------------
    // Semantic validation
    // ------------------------------------------------------------------

    let crate_names: HashSet<&str> = config.crates.iter().map(|c| c.name.as_str()).collect();

    // 1. depends_on references exist
    for c in &config.crates {
        if let Some(deps) = &c.depends_on {
            for dep in deps {
                if !crate_names.contains(dep.as_str()) {
                    errors.push(format!(
                        "crate '{}': depends_on '{}' does not exist",
                        c.name, dep
                    ));
                }
            }
        }
    }

    // 2. Cycle detection via DFS
    if let Some(cycle) = find_cycle(&config.crates) {
        errors.push(format!("depends_on cycle detected: {}", cycle.join(" → ")));
    }

    // 3. tag_template must contain {{ .Version }}
    for c in &config.crates {
        if !c.tag_template.is_empty()
            && !c.tag_template.contains("{{ .Version }}")
            && !c.tag_template.contains("{{.Version}}")
        {
            errors.push(format!(
                "crate '{}': tag_template '{}' must contain '{{{{ .Version }}}}'",
                c.name, c.tag_template
            ));
        }
    }

    // 4. copy_from references a binary in the same crate's builds
    for c in &config.crates {
        if let Some(builds) = &c.builds {
            let binaries: HashSet<&str> = builds.iter().map(|b| b.binary.as_str()).collect();
            for build in builds {
                if let Some(copy_from) = &build.copy_from
                    && !binaries.contains(copy_from.as_str())
                {
                    errors.push(format!(
                        "crate '{}': build binary '{}' has copy_from '{}' which is not a binary in this crate",
                        c.name, build.binary, copy_from
                    ));
                }
            }
        }
    }

    // 5. Target triples are recognized
    {
        let known_prefixes = ["x86_64", "aarch64", "i686", "armv7", "arm", "riscv64gc", "s390x", "powerpc64le"];
        let known_os = ["linux", "darwin", "apple", "windows", "freebsd", "netbsd", "android"];
        let mut check_triple = |triple: &str, context: &str| {
            let parts: Vec<&str> = triple.split('-').collect();
            let arch_ok = parts.first().is_some_and(|a| known_prefixes.iter().any(|p| a.starts_with(p)));
            let os_ok = known_os.iter().any(|os| triple.contains(os));
            if !arch_ok || !os_ok {
                warnings.push(format!("{}: unrecognized target triple '{}'", context, triple));
            }
        };
        if let Some(defaults) = &config.defaults
            && let Some(targets) = &defaults.targets {
            for t in targets {
                check_triple(t, "defaults.targets");
            }
        }
        for c in &config.crates {
            if let Some(builds) = &c.builds {
                for b in builds {
                    if let Some(targets) = &b.targets {
                        for t in targets {
                            check_triple(t, &format!("crate '{}' build '{}'", c.name, b.binary));
                        }
                    }
                }
            }
        }
    }

    // 6. Warn if changelog is disabled but has other fields configured
    if let Some(cl) = &config.changelog
        && cl.disable == Some(true)
    {
        let has_other = cl.sort.is_some()
            || cl.filters.is_some()
            || cl.groups.is_some()
            || cl.header.is_some()
            || cl.footer.is_some();
        if has_other {
            warnings.push(
                "changelog: disable is true but other changelog fields are also set (they will be ignored)".to_string(),
            );
        }
    }

    // 7. Warn if checksum is disabled but has other fields configured (global defaults)
    if let Some(defaults) = &config.defaults
        && let Some(cksum) = &defaults.checksum
        && cksum.disable == Some(true)
    {
        let has_other = cksum.algorithm.is_some() || cksum.name_template.is_some();
        if has_other {
            warnings.push(
                "defaults.checksum: disable is true but other checksum fields are also set (they will be ignored)".to_string(),
            );
        }
    }

    // 8. Warn if per-crate checksum is disabled but has other fields configured
    for c in &config.crates {
        if let Some(cksum) = &c.checksum
            && cksum.disable == Some(true)
        {
            let has_other = cksum.algorithm.is_some() || cksum.name_template.is_some();
            if has_other {
                warnings.push(format!(
                    "crate '{}': checksum disable is true but other checksum fields are also set (they will be ignored)",
                    c.name,
                ));
            }
        }
    }

    // 9. Crate path directories exist
    for c in &config.crates {
        if !c.path.is_empty() {
            let p = std::path::Path::new(&c.path);
            if !p.exists() {
                errors.push(format!(
                    "crate '{}': path '{}' does not exist",
                    c.name, c.path
                ));
            }
        }
    }

    // ------------------------------------------------------------------
    // Environment checks (warnings only)
    // ------------------------------------------------------------------

    if check_env {
        let needs_cross = config.crates.iter().any(|c| {
            use anodize_core::config::CrossStrategy;
            matches!(&c.cross, Some(CrossStrategy::Zigbuild) | Some(CrossStrategy::Auto))
                || config
                    .defaults
                    .as_ref()
                    .and_then(|d| d.cross.as_ref())
                    .is_some_and(|cs| matches!(cs, CrossStrategy::Zigbuild | CrossStrategy::Auto))
        });

        if needs_cross || config.crates.iter().any(|c| c.builds.is_some()) {
            if !tool_available("cargo-zigbuild") {
                warnings.push("cargo-zigbuild is not installed (needed for cross-compilation via zigbuild)".to_string());
            }
            if !tool_available("cross") {
                warnings.push("cross is not installed (needed for cross-compilation via cross)".to_string());
            }
        }

        let needs_docker = config.crates.iter().any(|c| c.docker.is_some());
        if needs_docker {
            if !tool_available("docker") {
                warnings.push("docker is not installed but docker sections are configured".to_string());
            } else {
                // Check for docker buildx
                let buildx_ok = std::process::Command::new("docker")
                    .args(["buildx", "version"])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if !buildx_ok {
                    warnings.push("docker buildx is not available but docker sections are configured".to_string());
                }
            }
        }

        let needs_release = config.crates.iter().any(|c| c.release.is_some());
        if needs_release && std::env::var("GITHUB_TOKEN").is_err() {
            warnings.push("GITHUB_TOKEN is not set but release sections are configured".to_string());
        }

        let needs_nfpm = config.crates.iter().any(|c| c.nfpm.is_some());
        if needs_nfpm && !tool_available("nfpm") {
            warnings.push("nfpm is not installed but nfpm sections are configured".to_string());
        }

        // GPG/cosign availability
        if !config.signs.is_empty() {
            for sign_cfg in &config.signs {
                let sign_cmd = sign_cfg.cmd.as_deref().unwrap_or("gpg");
                if !tool_available(sign_cmd) {
                    warnings.push(format!("'{}' is not installed but signs section is configured", sign_cmd));
                }
            }
        }
        if let Some(docker_signs) = &config.docker_signs {
            for ds in docker_signs {
                let cmd = ds.cmd.as_deref().unwrap_or("cosign");
                if !tool_available(cmd) {
                    warnings.push(format!("'{}' is not installed but docker_signs section is configured", cmd));
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Print results
    // ------------------------------------------------------------------

    for w in &warnings {
        eprintln!("  WARNING: {}", w);
    }

    if errors.is_empty() {
        eprintln!("  Config is valid.");
        Ok(())
    } else {
        for e in &errors {
            eprintln!("  ERROR: {}", e);
        }
        bail!("config validation failed with {} error(s)", errors.len());
    }
}

/// DFS-based cycle detection. Returns the cycle path if one exists.
pub fn find_cycle(crates: &[CrateConfig]) -> Option<Vec<String>> {
    let name_to_idx: HashMap<&str, usize> = crates
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.as_str(), i))
        .collect();

    // Build adjacency list
    let mut adj: Vec<Vec<usize>> = vec![vec![]; crates.len()];
    for (i, c) in crates.iter().enumerate() {
        if let Some(deps) = &c.depends_on {
            for dep in deps {
                if let Some(&j) = name_to_idx.get(dep.as_str()) {
                    // i depends on j → edge j→i in "needs" direction, but for cycle
                    // detection we walk: if i depends on j, j must be processed before i.
                    // We build edges i→j meaning "i needs j" to detect cycles in that graph.
                    adj[i].push(j);
                }
            }
        }
    }

    // 0 = unvisited, 1 = in-stack (gray), 2 = done (black)
    let mut color = vec![0u8; crates.len()];
    let mut parent = vec![usize::MAX; crates.len()];

    for start in 0..crates.len() {
        if color[start] != 0 {
            continue;
        }
        // Iterative DFS
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)]; // (node, adj_index)
        color[start] = 1;

        while let Some((node, adj_idx)) = stack.last_mut() {
            let node = *node;
            if *adj_idx < adj[node].len() {
                let next = adj[node][*adj_idx];
                *adj_idx += 1;
                match color[next] {
                    0 => {
                        color[next] = 1;
                        parent[next] = node;
                        stack.push((next, 0));
                    }
                    1 => {
                        // Back edge → cycle found; reconstruct path
                        let mut cycle = vec![crates[next].name.clone()];
                        let mut cur = node;
                        while cur != next {
                            cycle.push(crates[cur].name.clone());
                            cur = parent[cur];
                            if cur == usize::MAX {
                                break;
                            }
                        }
                        cycle.push(crates[next].name.clone());
                        cycle.reverse();
                        return Some(cycle);
                    }
                    _ => {} // already done
                }
            } else {
                color[node] = 2;
                stack.pop();
            }
        }
    }
    None
}

fn tool_available(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::{Config, CrateConfig};

    fn make_crate(name: &str, tag_template: &str, depends_on: Option<Vec<&str>>) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: tag_template.to_string(),
            depends_on: depends_on.map(|d| d.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    fn make_config(crates: Vec<CrateConfig>) -> Config {
        Config {
            project_name: "test".to_string(),
            crates,
            ..Default::default()
        }
    }

    // ---- Cycle detection tests ----

    #[test]
    fn test_no_cycle_linear() {
        let crates = vec![
            make_crate("a", "a-v{{ .Version }}", None),
            make_crate("b", "b-v{{ .Version }}", Some(vec!["a"])),
            make_crate("c", "c-v{{ .Version }}", Some(vec!["b"])),
        ];
        assert!(find_cycle(&crates).is_none());
    }

    #[test]
    fn test_cycle_two_nodes() {
        let crates = vec![
            make_crate("a", "a-v{{ .Version }}", Some(vec!["b"])),
            make_crate("b", "b-v{{ .Version }}", Some(vec!["a"])),
        ];
        let cycle = find_cycle(&crates);
        assert!(cycle.is_some(), "expected a cycle to be detected");
    }

    #[test]
    fn test_cycle_three_nodes() {
        let crates = vec![
            make_crate("a", "a-v{{ .Version }}", Some(vec!["c"])),
            make_crate("b", "b-v{{ .Version }}", Some(vec!["a"])),
            make_crate("c", "c-v{{ .Version }}", Some(vec!["b"])),
        ];
        let cycle = find_cycle(&crates);
        assert!(cycle.is_some(), "expected a cycle to be detected");
    }

    #[test]
    fn test_no_cycle_diamond() {
        let crates = vec![
            make_crate("base", "base-v{{ .Version }}", None),
            make_crate("left", "left-v{{ .Version }}", Some(vec!["base"])),
            make_crate("right", "right-v{{ .Version }}", Some(vec!["base"])),
            make_crate("top", "top-v{{ .Version }}", Some(vec!["left", "right"])),
        ];
        assert!(find_cycle(&crates).is_none());
    }

    // ---- tag_template validation tests ----

    #[test]
    fn test_tag_template_valid() {
        let config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        assert!(run_checks(&config, false).is_ok());
    }

    #[test]
    fn test_tag_template_missing_version() {
        let config = make_config(vec![make_crate("a", "release-tag", None)]);
        let result = run_checks(&config, false);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("validation failed"), "got: {}", msg);
    }

    #[test]
    fn test_tag_template_empty_skipped() {
        // Empty tag_template should not trigger the error (it's just unconfigured)
        let config = make_config(vec![make_crate("a", "", None)]);
        assert!(run_checks(&config, false).is_ok());
    }

    // ---- depends_on reference tests ----

    #[test]
    fn test_depends_on_missing_crate() {
        let config = make_config(vec![make_crate(
            "a",
            "a-v{{ .Version }}",
            Some(vec!["nonexistent"]),
        )]);
        let result = run_checks(&config, false);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("validation failed"), "got: {}", msg);
    }

    #[test]
    fn test_depends_on_cycle_fails() {
        let crates = vec![
            make_crate("a", "a-v{{ .Version }}", Some(vec!["b"])),
            make_crate("b", "b-v{{ .Version }}", Some(vec!["a"])),
        ];
        let config = make_config(crates);
        let result = run_checks(&config, false);
        assert!(result.is_err());
    }

    // ---- copy_from tests ----

    #[test]
    fn test_copy_from_valid() {
        use anodize_core::config::BuildConfig;
        let mut c = make_crate("a", "a-v{{ .Version }}", None);
        c.builds = Some(vec![
            BuildConfig { binary: "a".to_string(), ..Default::default() },
            BuildConfig {
                binary: "b".to_string(),
                copy_from: Some("a".to_string()),
                ..Default::default()
            },
        ]);
        let config = make_config(vec![c]);
        assert!(run_checks(&config, false).is_ok());
    }

    #[test]
    fn test_copy_from_invalid() {
        use anodize_core::config::BuildConfig;
        let mut c = make_crate("a", "a-v{{ .Version }}", None);
        c.builds = Some(vec![BuildConfig {
            binary: "b".to_string(),
            copy_from: Some("nonexistent".to_string()),
            ..Default::default()
        }]);
        let config = make_config(vec![c]);
        let result = run_checks(&config, false);
        assert!(result.is_err());
    }

    // ---- Contradictory config warning tests ----

    #[test]
    fn test_check_changelog_disabled_with_other_fields_passes() {
        use anodize_core::config::{ChangelogConfig, ChangelogGroup};
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.changelog = Some(ChangelogConfig {
            disable: Some(true),
            sort: Some("desc".to_string()),
            header: Some("header".to_string()),
            footer: None,
            filters: None,
            groups: Some(vec![ChangelogGroup {
                title: "Features".to_string(),
                regexp: Some("^feat".to_string()),
                order: Some(0),
            }]),
        });
        // Should pass (warnings only, not errors)
        assert!(run_checks(&config, false).is_ok());
    }

    #[test]
    fn test_check_checksum_disabled_with_other_fields_passes() {
        use anodize_core::config::{ChecksumConfig, Defaults};
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.defaults = Some(Defaults {
            checksum: Some(ChecksumConfig {
                disable: Some(true),
                algorithm: Some("sha512".to_string()),
                name_template: None,
            }),
            ..Default::default()
        });
        // Should pass (warnings only, not errors)
        assert!(run_checks(&config, false).is_ok());
    }

    #[test]
    fn test_check_per_crate_checksum_disabled_with_other_fields_passes() {
        use anodize_core::config::ChecksumConfig;
        let mut c = make_crate("a", "a-v{{ .Version }}", None);
        c.checksum = Some(ChecksumConfig {
            disable: Some(true),
            algorithm: Some("sha512".to_string()),
            name_template: Some("checksums.txt".to_string()),
        });
        let config = make_config(vec![c]);
        // Should pass (warnings only, not errors)
        assert!(run_checks(&config, false).is_ok());
    }
}
