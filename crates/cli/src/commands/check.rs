use super::helpers;
use crate::pipeline;
use anodize_core::config::{Config, CrateConfig};
use anodize_core::log::{StageLogger, Verbosity};
use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub fn run(
    config_override: Option<&Path>,
    workspace: Option<&str>,
    verbose: bool,
    debug: bool,
    quiet: bool,
) -> Result<()> {
    let log = StageLogger::new("check", Verbosity::from_flags(quiet, verbose, debug));

    let path = pipeline::find_config(config_override)?;
    log.verbose(&format!("loading config from {}", path.display()));
    let config = pipeline::load_config(&path)?;

    // Always validate the raw config first
    log.status("validating configuration");
    run_checks(&config, true, &log)?;

    // When --workspace is specified, also validate the resolved (overlaid) config
    if let Some(ws_name) = workspace {
        let ws = super::release::resolve_workspace(&config, ws_name)?;
        let mut resolved = config.clone();
        helpers::apply_workspace_overlay(&mut resolved, ws);
        log.status(&format!(
            "validating resolved config for workspace '{}'",
            ws_name
        ));
        run_checks(&resolved, true, &log)?;
    }

    Ok(())
}

/// Core validation logic. `check_env` controls whether env/tool checks are run
/// (so tests can skip them).
pub fn run_checks(config: &Config, check_env: bool, log: &StageLogger) -> Result<()> {
    let mut errors: Vec<String> = vec![];
    let mut warnings: Vec<String> = vec![];

    // ------------------------------------------------------------------
    // Semantic validation
    // ------------------------------------------------------------------

    let crate_names: HashSet<&str> = config.crates.iter().map(|c| c.name.as_str()).collect();

    // 0a. Workspace names must be unique and non-empty
    if let Some(ref workspaces) = config.workspaces {
        let mut seen_names: HashSet<&str> = HashSet::new();
        for (i, ws) in workspaces.iter().enumerate() {
            if ws.name.trim().is_empty() {
                errors.push(format!("workspace at index {}: name must not be empty", i));
            } else if !seen_names.insert(ws.name.as_str()) {
                errors.push(format!("duplicate workspace name '{}'", ws.name));
            }
        }

        // Validate workspace crate names are non-empty and unique within each workspace
        for ws in workspaces {
            let mut ws_crate_names: HashSet<&str> = HashSet::new();
            for (i, c) in ws.crates.iter().enumerate() {
                if c.name.trim().is_empty() {
                    errors.push(format!(
                        "workspace '{}': crate at index {}: name must not be empty",
                        ws.name, i
                    ));
                } else if !ws_crate_names.insert(c.name.as_str()) {
                    errors.push(format!(
                        "workspace '{}': duplicate crate name '{}'",
                        ws.name, c.name
                    ));
                }
            }
            // Validate tag_template in workspace crates
            for c in &ws.crates {
                validate_tag_template(
                    &c.tag_template,
                    &format!("workspace '{}': crate '{}'", ws.name, c.name),
                    &mut errors,
                );
            }
            // Validate depends_on references within workspace crates
            for c in &ws.crates {
                if let Some(deps) = &c.depends_on {
                    for dep in deps {
                        if !ws_crate_names.contains(dep.as_str()) {
                            errors.push(format!(
                                "workspace '{}': crate '{}': depends_on '{}' does not exist",
                                ws.name, c.name, dep
                            ));
                        }
                    }
                }
            }
        }
    }

    // 0. Crate names must not be empty
    for (i, c) in config.crates.iter().enumerate() {
        if c.name.trim().is_empty() {
            errors.push(format!("crate at index {}: name must not be empty", i));
        }
    }

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

    // 3. tag_template must contain {{ .Version }} or {{ Version }} (Tera-native)
    for c in &config.crates {
        validate_tag_template(&c.tag_template, &format!("crate '{}'", c.name), &mut errors);
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
        let known_prefixes = [
            "x86_64",
            "aarch64",
            "i686",
            "armv7",
            "arm",
            "riscv64gc",
            "s390x",
            "powerpc64le",
        ];
        let known_os = [
            "linux", "darwin", "apple", "windows", "freebsd", "netbsd", "android",
        ];
        let mut check_triple = |triple: &str, context: &str| {
            let parts: Vec<&str> = triple.split('-').collect();
            let arch_ok = parts
                .first()
                .is_some_and(|a| known_prefixes.iter().any(|p| a.starts_with(p)));
            let os_ok = known_os.iter().any(|os| triple.contains(os));
            if !arch_ok || !os_ok {
                warnings.push(format!(
                    "{}: unrecognized target triple '{}'",
                    context, triple
                ));
            }
        };
        if let Some(defaults) = &config.defaults
            && let Some(targets) = &defaults.targets
        {
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
        && cl.disable == Some(anodize_core::config::StringOrBool::Bool(true))
    {
        let has_other = cl.sort.is_some()
            || cl.filters.is_some()
            || cl.groups.is_some()
            || cl.header.is_some()
            || cl.footer.is_some()
            || cl.use_source.is_some()
            || cl.abbrev.is_some();
        if has_other {
            warnings.push(
                "changelog: disable is true but other changelog fields are also set (they will be ignored)".to_string(),
            );
        }
    }

    // 6b. Validate changelog use_source value
    if let Some(cl) = &config.changelog
        && let Some(ref use_source) = cl.use_source
        && use_source != "git"
        && use_source != "github-native"
    {
        warnings.push(format!(
            "changelog: unrecognized 'use' value '{}' (valid: git, github-native)",
            use_source
        ));
    }

    // 7. Warn if checksum is disabled but has other fields configured (global defaults)
    if let Some(defaults) = &config.defaults
        && let Some(cksum) = &defaults.checksum
        && cksum.disable.as_ref().is_some_and(|d| d.as_bool())
    {
        let has_other = cksum.algorithm.is_some()
            || cksum.name_template.is_some()
            || cksum.extra_files.is_some()
            || cksum.ids.is_some();
        if has_other {
            warnings.push(
                "defaults.checksum: disable is true but other checksum fields are also set (they will be ignored)".to_string(),
            );
        }
    }

    // 8. Warn if per-crate checksum is disabled but has other fields configured
    for c in &config.crates {
        if let Some(cksum) = &c.checksum
            && cksum.disable.as_ref().is_some_and(|d| d.as_bool())
        {
            let has_other = cksum.algorithm.is_some()
                || cksum.name_template.is_some()
                || cksum.extra_files.is_some()
                || cksum.ids.is_some();
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

    // 10. Validate sign artifact filter values
    let valid_artifact_filters = [
        "none", "all", "checksum", "source", "archive", "binary", "package",
    ];
    for sign_cfg in &config.signs {
        if let Some(ref filter) = sign_cfg.artifacts
            && !valid_artifact_filters.contains(&filter.as_str())
        {
            warnings.push(format!(
                "signs: unrecognized artifacts filter '{}' (valid: {})",
                filter,
                valid_artifact_filters.join(", ")
            ));
        }
    }

    // 11. Validate checksum algorithm values
    let valid_algorithms = [
        "sha1", "sha224", "sha256", "sha384", "sha512", "blake2b", "blake2s",
    ];
    if let Some(defaults) = &config.defaults
        && let Some(cksum) = &defaults.checksum
        && let Some(ref algo) = cksum.algorithm
        && !valid_algorithms.contains(&algo.as_str())
    {
        warnings.push(format!(
            "defaults.checksum: unrecognized algorithm '{}' (valid: {})",
            algo,
            valid_algorithms.join(", ")
        ));
    }
    for c in &config.crates {
        if let Some(cksum) = &c.checksum
            && let Some(ref algo) = cksum.algorithm
            && !valid_algorithms.contains(&algo.as_str())
        {
            warnings.push(format!(
                "crate '{}': unrecognized checksum algorithm '{}' (valid: {})",
                c.name,
                algo,
                valid_algorithms.join(", ")
            ));
        }
    }

    // 12. Validate source.format
    if let Some(ref source) = config.source
        && let Some(ref fmt) = source.format
    {
        let valid_source_formats = ["tar.gz", "tgz", "tar", "zip"];
        if !valid_source_formats.contains(&fmt.as_str()) {
            errors.push(format!(
                "source: unrecognized format '{}' (valid: {})",
                fmt,
                valid_source_formats.join(", ")
            ));
        }
    }

    // 13. Validate sbom configs
    for (i, sbom) in config.sboms.iter().enumerate() {
        let idx_str = i.to_string();
        let label = sbom
            .id
            .as_deref()
            .unwrap_or_else(|| if i == 0 { "default" } else { &idx_str });
        if let Some(ref artifacts) = sbom.artifacts {
            let valid = [
                "source",
                "archive",
                "binary",
                "package",
                "diskimage",
                "installer",
                "any",
            ];
            if !valid.contains(&artifacts.as_str()) {
                errors.push(format!(
                    "sboms[{}]: invalid artifacts type '{}' (valid: {})",
                    label,
                    artifacts,
                    valid.join(", ")
                ));
            }
        }
    }

    // 14. Validate blob configs
    let valid_blob_providers = ["s3", "gs", "gcs", "azblob", "azure"];
    for c in &config.crates {
        if let Some(ref blobs) = c.blobs {
            for (i, blob) in blobs.iter().enumerate() {
                let idx = i.to_string();
                let label = blob.id.as_deref().unwrap_or(&idx);
                if blob.provider.is_empty() {
                    errors.push(format!(
                        "crate '{}' blobs[{}]: provider is required",
                        c.name, label
                    ));
                } else if !valid_blob_providers.contains(&blob.provider.as_str()) {
                    errors.push(format!(
                        "crate '{}' blobs[{}]: unrecognized provider '{}' (valid: {})",
                        c.name,
                        label,
                        blob.provider,
                        valid_blob_providers.join(", ")
                    ));
                }
                if blob.bucket.is_empty() {
                    errors.push(format!(
                        "crate '{}' blobs[{}]: bucket is required",
                        c.name, label
                    ));
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Environment checks (warnings only)
    // ------------------------------------------------------------------

    if check_env {
        let needs_cross = config.crates.iter().any(|c| {
            use anodize_core::config::CrossStrategy;
            matches!(
                &c.cross,
                Some(CrossStrategy::Zigbuild) | Some(CrossStrategy::Auto)
            ) || config
                .defaults
                .as_ref()
                .and_then(|d| d.cross.as_ref())
                .is_some_and(|cs| matches!(cs, CrossStrategy::Zigbuild | CrossStrategy::Auto))
        });

        if needs_cross || config.crates.iter().any(|c| c.builds.is_some()) {
            if !tool_available("cargo-zigbuild") {
                warnings.push(
                    "cargo-zigbuild is not installed (needed for cross-compilation via zigbuild)"
                        .to_string(),
                );
            }
            if !tool_available("cross") {
                warnings.push(
                    "cross is not installed (needed for cross-compilation via cross)".to_string(),
                );
            }
        }

        let needs_docker = config.crates.iter().any(|c| c.docker.is_some());
        if needs_docker {
            if !tool_available("docker") {
                warnings
                    .push("docker is not installed but docker sections are configured".to_string());
            } else {
                // Check for docker buildx
                let buildx_ok = std::process::Command::new("docker")
                    .args(["buildx", "version"])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if !buildx_ok {
                    warnings.push(
                        "docker buildx is not available but docker sections are configured"
                            .to_string(),
                    );
                }
            }
        }

        let needs_release = config.crates.iter().any(|c| c.release.is_some());
        if needs_release
            && std::env::var("ANODIZE_GITHUB_TOKEN").is_err()
            && std::env::var("GITHUB_TOKEN").is_err()
        {
            warnings.push(
                "no GitHub token found but release sections are configured; set GITHUB_TOKEN or ANODIZE_GITHUB_TOKEN"
                    .to_string(),
            );
        }

        let needs_nfpm = config.crates.iter().any(|c| c.nfpm.is_some());
        if needs_nfpm && !tool_available("nfpm") {
            warnings.push("nfpm is not installed but nfpm sections are configured".to_string());
        }

        // Blob storage: cloud credentials are validated at upload time by the SDK.
        // No CLI tools needed — object_store handles S3/GCS/Azure natively.
        // We still validate config correctness (provider, bucket) above.

        // GPG/cosign availability
        if !config.signs.is_empty() {
            for sign_cfg in &config.signs {
                let sign_cmd = sign_cfg.cmd.as_deref().unwrap_or("gpg");
                if !tool_available(sign_cmd) {
                    warnings.push(format!(
                        "'{}' is not installed but signs section is configured",
                        sign_cmd
                    ));
                }
            }
        }
        if let Some(docker_signs) = &config.docker_signs {
            for ds in docker_signs {
                let cmd = ds.cmd.as_deref().unwrap_or("cosign");
                if !tool_available(cmd) {
                    warnings.push(format!(
                        "'{}' is not installed but docker_signs section is configured",
                        cmd
                    ));
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Print results
    // ------------------------------------------------------------------

    for w in &warnings {
        log.warn(w);
    }

    if errors.is_empty() {
        log.status("Config is valid.");
        Ok(())
    } else {
        for e in &errors {
            log.error(e);
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

/// Validate that a tag_template contains a Version placeholder.
/// `context` is a human-readable prefix for the error message (e.g. "crate 'foo'" or
/// "workspace 'ws': crate 'bar'").
fn validate_tag_template(tag_template: &str, context: &str, errors: &mut Vec<String>) {
    if !tag_template.is_empty() && !anodize_core::git::has_version_placeholder(tag_template) {
        errors.push(format!(
            "{}: tag_template '{}' must contain '{{{{ .Version }}}}' or '{{{{ Version }}}}' \
             (e.g. 'v{{{{ .Version }}}}' or 'myapp-v{{{{ Version }}}}')",
            context, tag_template
        ));
    }
}

fn tool_available(name: &str) -> bool {
    anodize_core::util::find_binary(name)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::{Config, CrateConfig, WorkspaceConfig};

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

    fn test_logger() -> StageLogger {
        StageLogger::new("check", Verbosity::Quiet)
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
        assert!(run_checks(&config, false, &test_logger()).is_ok());
    }

    #[test]
    fn test_tag_template_missing_version() {
        let config = make_config(vec![make_crate("a", "release-tag", None)]);
        let result = run_checks(&config, false, &test_logger());
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("validation failed"), "got: {}", msg);
    }

    #[test]
    fn test_tag_template_empty_skipped() {
        // Empty tag_template should not trigger the error (it's just unconfigured)
        let config = make_config(vec![make_crate("a", "", None)]);
        assert!(run_checks(&config, false, &test_logger()).is_ok());
    }

    // ---- depends_on reference tests ----

    #[test]
    fn test_depends_on_missing_crate() {
        let config = make_config(vec![make_crate(
            "a",
            "a-v{{ .Version }}",
            Some(vec!["nonexistent"]),
        )]);
        let result = run_checks(&config, false, &test_logger());
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
        let result = run_checks(&config, false, &test_logger());
        assert!(result.is_err());
    }

    // ---- copy_from tests ----

    #[test]
    fn test_copy_from_valid() {
        use anodize_core::config::BuildConfig;
        let mut c = make_crate("a", "a-v{{ .Version }}", None);
        c.builds = Some(vec![
            BuildConfig {
                binary: "a".to_string(),
                ..Default::default()
            },
            BuildConfig {
                binary: "b".to_string(),
                copy_from: Some("a".to_string()),
                ..Default::default()
            },
        ]);
        let config = make_config(vec![c]);
        assert!(run_checks(&config, false, &test_logger()).is_ok());
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
        let result = run_checks(&config, false, &test_logger());
        assert!(result.is_err());
    }

    // ---- Contradictory config warning tests ----

    #[test]
    fn test_check_changelog_disabled_with_other_fields_passes() {
        use anodize_core::config::{ChangelogConfig, ChangelogGroup};
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.changelog = Some(ChangelogConfig {
            disable: Some(anodize_core::config::StringOrBool::Bool(true)),
            sort: Some("desc".to_string()),
            header: Some("header".to_string()),
            groups: Some(vec![ChangelogGroup {
                title: "Features".to_string(),
                regexp: Some("^feat".to_string()),
                order: Some(0),
                groups: None,
            }]),
            ..Default::default()
        });
        // Should pass (warnings only, not errors)
        assert!(run_checks(&config, false, &test_logger()).is_ok());
    }

    #[test]
    fn test_check_checksum_disabled_with_other_fields_passes() {
        use anodize_core::config::{ChecksumConfig, Defaults, StringOrBool};
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.defaults = Some(Defaults {
            checksum: Some(ChecksumConfig {
                disable: Some(StringOrBool::Bool(true)),
                algorithm: Some("sha512".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        // Should pass (warnings only, not errors)
        assert!(run_checks(&config, false, &test_logger()).is_ok());
    }

    // ---- Empty crate name validation tests ----

    #[test]
    fn test_empty_crate_name_fails() {
        let config = make_config(vec![make_crate("", "v{{ .Version }}", None)]);
        let result = run_checks(&config, false, &test_logger());
        assert!(result.is_err(), "empty crate name should fail validation");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("validation failed"), "got: {}", msg);
    }

    #[test]
    fn test_whitespace_only_crate_name_fails() {
        let config = make_config(vec![make_crate("  ", "v{{ .Version }}", None)]);
        let result = run_checks(&config, false, &test_logger());
        assert!(
            result.is_err(),
            "whitespace-only crate name should fail validation"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("1 error(s)"),
            "error should report 1 validation error, got: {msg}"
        );
    }

    // ---- tag_template compact spacing variant tests ----

    #[test]
    fn test_tag_template_compact_version_accepted() {
        // {{.Version}} without spaces should also be accepted
        let config = make_config(vec![make_crate("a", "v{{.Version}}", None)]);
        assert!(run_checks(&config, false, &test_logger()).is_ok());
    }

    #[test]
    fn test_tag_template_tera_native_version_accepted() {
        // {{ Version }} (Tera-native, no dot) should also be accepted
        let config = make_config(vec![make_crate("a", "v{{ Version }}", None)]);
        assert!(run_checks(&config, false, &test_logger()).is_ok());
    }

    #[test]
    fn test_tag_template_tera_native_compact_version_accepted() {
        // {{Version}} (Tera-native, no dot, no spaces) should also be accepted
        let config = make_config(vec![make_crate("a", "v{{Version}}", None)]);
        assert!(run_checks(&config, false, &test_logger()).is_ok());
    }

    #[test]
    fn test_tag_template_missing_version_with_other_placeholder() {
        // Has a placeholder but not {{ .Version }}
        let config = make_config(vec![make_crate("a", "{{ .Tag }}-release", None)]);
        let result = run_checks(&config, false, &test_logger());
        assert!(
            result.is_err(),
            "tag_template without Version placeholder should fail"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("1 error(s)"),
            "error should report 1 validation error, got: {msg}"
        );
    }

    // ---- Multiple validation errors test ----

    #[test]
    fn test_multiple_validation_errors_reported() {
        let crates = vec![
            make_crate("", "v{{ .Version }}", None), // empty name
            make_crate("b", "bad-tag", Some(vec!["nonexistent"])), // missing dep + bad template
        ];
        let config = make_config(crates);
        let result = run_checks(&config, false, &test_logger());
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        // Should report exactly 3 errors: empty name, missing dep, bad tag_template
        assert!(
            msg.contains("3 error(s)"),
            "should report 3 error(s), got: {}",
            msg
        );
    }

    #[test]
    fn test_check_per_crate_checksum_disabled_with_other_fields_passes() {
        use anodize_core::config::{ChecksumConfig, StringOrBool};
        let mut c = make_crate("a", "a-v{{ .Version }}", None);
        c.checksum = Some(ChecksumConfig {
            disable: Some(StringOrBool::Bool(true)),
            algorithm: Some("sha512".to_string()),
            name_template: Some("checksums.txt".to_string()),
            ..Default::default()
        });
        let config = make_config(vec![c]);
        // Should pass (warnings only, not errors)
        assert!(run_checks(&config, false, &test_logger()).is_ok());
    }

    // ---- Workspace validation tests ----

    #[test]
    fn test_workspace_names_unique_passes() {
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.workspaces = Some(vec![
            WorkspaceConfig {
                name: "frontend".to_string(),
                crates: vec![make_crate("fe", "fe-v{{ .Version }}", None)],
                ..Default::default()
            },
            WorkspaceConfig {
                name: "backend".to_string(),
                crates: vec![make_crate("be", "be-v{{ .Version }}", None)],
                ..Default::default()
            },
        ]);
        assert!(run_checks(&config, false, &test_logger()).is_ok());
    }

    #[test]
    fn test_workspace_duplicate_name_fails() {
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.workspaces = Some(vec![
            WorkspaceConfig {
                name: "dup".to_string(),
                crates: vec![make_crate("x", "x-v{{ .Version }}", None)],
                ..Default::default()
            },
            WorkspaceConfig {
                name: "dup".to_string(),
                crates: vec![make_crate("y", "y-v{{ .Version }}", None)],
                ..Default::default()
            },
        ]);
        let result = run_checks(&config, false, &test_logger());
        assert!(result.is_err(), "duplicate workspace names should fail");
    }

    #[test]
    fn test_workspace_empty_name_fails() {
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.workspaces = Some(vec![WorkspaceConfig {
            name: "".to_string(),
            crates: vec![make_crate("x", "x-v{{ .Version }}", None)],
            ..Default::default()
        }]);
        let result = run_checks(&config, false, &test_logger());
        assert!(result.is_err(), "empty workspace name should fail");
    }

    #[test]
    fn test_workspace_crate_empty_name_fails() {
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.workspaces = Some(vec![WorkspaceConfig {
            name: "ws1".to_string(),
            crates: vec![make_crate("", "v{{ .Version }}", None)],
            ..Default::default()
        }]);
        let result = run_checks(&config, false, &test_logger());
        assert!(result.is_err(), "empty crate name in workspace should fail");
    }

    #[test]
    fn test_workspace_crate_bad_tag_template_fails() {
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.workspaces = Some(vec![WorkspaceConfig {
            name: "ws1".to_string(),
            crates: vec![make_crate("x", "no-version-here", None)],
            ..Default::default()
        }]);
        let result = run_checks(&config, false, &test_logger());
        assert!(
            result.is_err(),
            "bad tag_template in workspace crate should fail"
        );
    }

    #[test]
    fn test_no_workspaces_passes() {
        let config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        assert!(run_checks(&config, false, &test_logger()).is_ok());
    }

    #[test]
    fn test_workspace_duplicate_crate_name_fails() {
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.workspaces = Some(vec![WorkspaceConfig {
            name: "ws1".to_string(),
            crates: vec![
                make_crate("dup", "dup-v{{ .Version }}", None),
                make_crate("dup", "dup-v{{ .Version }}", None),
            ],
            ..Default::default()
        }]);
        let result = run_checks(&config, false, &test_logger());
        assert!(
            result.is_err(),
            "duplicate crate names within a workspace should fail"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("1 error(s)"),
            "should report 1 validation error for duplicate crate name: {}",
            msg
        );
    }

    #[test]
    fn test_workspace_depends_on_missing_fails() {
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.workspaces = Some(vec![WorkspaceConfig {
            name: "ws1".to_string(),
            crates: vec![make_crate(
                "x",
                "x-v{{ .Version }}",
                Some(vec!["nonexistent"]),
            )],
            ..Default::default()
        }]);
        let result = run_checks(&config, false, &test_logger());
        assert!(
            result.is_err(),
            "workspace crate with missing depends_on should fail"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("1 error(s)"),
            "should report 1 validation error for missing depends_on: {}",
            msg
        );
    }

    #[test]
    fn test_workspace_depends_on_valid_passes() {
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.workspaces = Some(vec![WorkspaceConfig {
            name: "ws1".to_string(),
            crates: vec![
                make_crate("lib", "lib-v{{ .Version }}", None),
                make_crate("app", "app-v{{ .Version }}", Some(vec!["lib"])),
            ],
            ..Default::default()
        }]);
        assert!(
            run_checks(&config, false, &test_logger()).is_ok(),
            "valid depends_on within workspace should pass"
        );
    }

    // ---- Source/SBOM format validation tests ----

    #[test]
    fn test_invalid_source_format_fails() {
        use anodize_core::config::SourceConfig;
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.source = Some(SourceConfig {
            enabled: Some(true),
            format: Some("tar.bz2".to_string()),
            name_template: None,
            prefix_template: None,
            files: vec![],
        });
        let result = run_checks(&config, false, &test_logger());
        assert!(result.is_err(), "invalid source format should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("validation failed"), "got: {}", msg);
    }

    #[test]
    fn test_valid_source_formats_pass() {
        use anodize_core::config::SourceConfig;
        for fmt in &["tar.gz", "tgz", "tar", "zip"] {
            let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
            config.source = Some(SourceConfig {
                enabled: Some(true),
                format: Some(fmt.to_string()),
                name_template: None,
                prefix_template: None,
                files: vec![],
            });
            assert!(
                run_checks(&config, false, &test_logger()).is_ok(),
                "source format '{}' should pass",
                fmt
            );
        }
    }

    #[test]
    fn test_invalid_sbom_artifacts_fails() {
        use anodize_core::config::SbomConfig;
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.sboms = vec![SbomConfig {
            artifacts: Some("invalid".to_string()),
            ..Default::default()
        }];
        let result = run_checks(&config, false, &test_logger());
        assert!(result.is_err(), "invalid sbom artifacts should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("validation failed"), "got: {}", msg);
    }

    #[test]
    fn test_valid_sbom_artifacts_pass() {
        use anodize_core::config::SbomConfig;
        for art in &[
            "source",
            "archive",
            "binary",
            "package",
            "diskimage",
            "installer",
            "any",
        ] {
            let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
            config.sboms = vec![SbomConfig {
                artifacts: Some(art.to_string()),
                ..Default::default()
            }];
            assert!(
                run_checks(&config, false, &test_logger()).is_ok(),
                "sbom artifacts '{}' should pass",
                art
            );
        }
    }

    // -----------------------------------------------------------------------
    // Blob config validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_blob_config_valid_provider() {
        use anodize_core::config::BlobConfig;
        for provider in &["s3", "gcs", "gs", "azblob", "azure"] {
            let mut config = make_config(vec![make_crate("a", "v{{ .Version }}", None)]);
            config.crates[0].blobs = Some(vec![BlobConfig {
                provider: provider.to_string(),
                bucket: "my-bucket".to_string(),
                ..Default::default()
            }]);
            assert!(
                run_checks(&config, false, &test_logger()).is_ok(),
                "blob provider '{}' should pass",
                provider
            );
        }
    }

    #[test]
    fn test_blob_config_invalid_provider() {
        use anodize_core::config::BlobConfig;
        let mut config = make_config(vec![make_crate("a", "v{{ .Version }}", None)]);
        config.crates[0].blobs = Some(vec![BlobConfig {
            provider: "dropbox".to_string(),
            bucket: "b".to_string(),
            ..Default::default()
        }]);
        let result = run_checks(&config, false, &test_logger());
        assert!(result.is_err(), "invalid blob provider should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("validation failed"), "got: {}", msg);
    }

    #[test]
    fn test_blob_config_empty_provider() {
        use anodize_core::config::BlobConfig;
        let mut config = make_config(vec![make_crate("a", "v{{ .Version }}", None)]);
        config.crates[0].blobs = Some(vec![BlobConfig {
            provider: String::new(),
            bucket: "b".to_string(),
            ..Default::default()
        }]);
        let result = run_checks(&config, false, &test_logger());
        assert!(result.is_err(), "empty blob provider should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("validation failed"), "got: {}", msg);
    }

    #[test]
    fn test_blob_config_empty_bucket() {
        use anodize_core::config::BlobConfig;
        let mut config = make_config(vec![make_crate("a", "v{{ .Version }}", None)]);
        config.crates[0].blobs = Some(vec![BlobConfig {
            provider: "s3".to_string(),
            bucket: String::new(),
            ..Default::default()
        }]);
        let result = run_checks(&config, false, &test_logger());
        assert!(result.is_err(), "empty blob bucket should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("validation failed"), "got: {}", msg);
    }

    #[test]
    fn test_blob_config_id_in_error_label() {
        use anodize_core::config::BlobConfig;
        let mut config = make_config(vec![make_crate("a", "v{{ .Version }}", None)]);
        config.crates[0].blobs = Some(vec![BlobConfig {
            id: Some("my-upload".to_string()),
            provider: "invalid".to_string(),
            bucket: "b".to_string(),
            ..Default::default()
        }]);
        let result = run_checks(&config, false, &test_logger());
        assert!(result.is_err(), "invalid provider with id should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("validation failed"), "got: {}", msg);
    }
}
