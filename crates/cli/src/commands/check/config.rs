use super::super::helpers;
use crate::pipeline;
use anodizer_core::config::{Config, CrateConfig};
use anodizer_core::log::{StageLogger, Verbosity};
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

    let path = pipeline::find_config_with_logger(config_override, Some(&log))?;
    log.verbose(&format!("loading config from {}", path.display()));
    let mut config = pipeline::load_config(&path)?;

    // Auto-infer project_name from Cargo.toml when not set in config so
    // check validates the same project_name the release pipeline would see.
    helpers::infer_project_name(&mut config, &log);

    // Always validate the raw config first
    log.status("validating configuration");
    run_checks(&config, true, &log)?;

    // When --workspace is specified, also validate the resolved (overlaid) config
    if let Some(ws_name) = workspace {
        let ws = super::super::release::resolve_workspace(&config, ws_name)?;
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

    let all_crate_names = flatten_crate_names(config);

    check_workspaces(config, &all_crate_names, &mut errors);
    check_top_level_crate_names(config, &mut errors);
    check_top_level_depends_on(config, &all_crate_names, &mut errors);
    check_cycles(config, &mut errors);
    check_top_level_tag_templates(config, &mut errors);
    check_copy_from(config, &mut errors);
    check_target_triples(config, &mut warnings);
    check_changelog(config, &mut warnings);
    check_announce_secret_exposure(config, &mut warnings);
    check_checksum_skip_conflicts(config, &mut warnings);
    check_crate_paths(config, &mut errors);
    check_sign_artifact_filters(config, &mut warnings);
    check_checksum_algorithms(config, &mut warnings);
    check_source_format(config, &mut errors);
    check_sbom_configs(config, &mut errors);
    check_blob_configs(config, &mut errors);

    if check_env {
        check_environment(config, &mut warnings);
    }

    report_results(log, &warnings, &errors)
}

/// Flatten the crate-name set across top-level crates and every workspace's
/// crates. The release engine topo-sorts using this flattened set, so
/// `depends_on` references can cross workspace boundaries at release time
/// (e.g. a workspace crate depending on a crate in another workspace). The
/// validator must mirror that resolution to avoid false positives.
fn flatten_crate_names(config: &Config) -> HashSet<&str> {
    let mut s: HashSet<&str> = config.crates.iter().map(|c| c.name.as_str()).collect();
    if let Some(ref workspaces) = config.workspaces {
        for ws in workspaces {
            for c in &ws.crates {
                s.insert(c.name.as_str());
            }
        }
    }
    s
}

/// Validate workspace names (non-empty, unique) plus per-workspace crate
/// names, tag templates, and `depends_on` references (resolved against the
/// flattened crate set so cross-workspace refs are accepted).
fn check_workspaces(config: &Config, all_crate_names: &HashSet<&str>, errors: &mut Vec<String>) {
    let Some(ref workspaces) = config.workspaces else {
        return;
    };

    let mut seen_names: HashSet<&str> = HashSet::new();
    for (i, ws) in workspaces.iter().enumerate() {
        if ws.name.trim().is_empty() {
            errors.push(format!("workspace at index {}: name must not be empty", i));
        } else if !seen_names.insert(ws.name.as_str()) {
            errors.push(format!("duplicate workspace name '{}'", ws.name));
        }
    }

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
        for c in &ws.crates {
            validate_tag_template(
                &c.tag_template,
                &format!("workspace '{}': crate '{}'", ws.name, c.name),
                errors,
            );
        }
        for c in &ws.crates {
            if let Some(deps) = &c.depends_on {
                for dep in deps {
                    if !all_crate_names.contains(dep.as_str()) {
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

/// Top-level crate names must be non-empty.
fn check_top_level_crate_names(config: &Config, errors: &mut Vec<String>) {
    for (i, c) in config.crates.iter().enumerate() {
        if c.name.trim().is_empty() {
            errors.push(format!("crate at index {}: name must not be empty", i));
        }
    }
}

/// Top-level `depends_on` references must resolve against the flattened
/// crate set so a top-level crate can depend on a crate that lives in a
/// workspace.
fn check_top_level_depends_on(
    config: &Config,
    all_crate_names: &HashSet<&str>,
    errors: &mut Vec<String>,
) {
    for c in &config.crates {
        if let Some(deps) = &c.depends_on {
            for dep in deps {
                if !all_crate_names.contains(dep.as_str()) {
                    errors.push(format!(
                        "crate '{}': depends_on '{}' does not exist",
                        c.name, dep
                    ));
                }
            }
        }
    }
}

/// DFS-based cycle detection across top-level crates.
fn check_cycles(config: &Config, errors: &mut Vec<String>) {
    if let Some(cycle) = find_cycle(&config.crates) {
        errors.push(format!("depends_on cycle detected: {}", cycle.join(" → ")));
    }
}

/// Top-level `tag_template` must contain `{{ .Version }}` or `{{ Version }}`
/// (Tera-native).
fn check_top_level_tag_templates(config: &Config, errors: &mut Vec<String>) {
    for c in &config.crates {
        validate_tag_template(&c.tag_template, &format!("crate '{}'", c.name), errors);
    }
}

/// Each build's `copy_from` must reference a binary defined in the same
/// crate's builds. The effective binary name falls back to the crate name
/// when the per-build `binary` field is omitted (e.g. when defaults supply a
/// template without `binary:`).
fn check_copy_from(config: &Config, errors: &mut Vec<String>) {
    for c in &config.crates {
        if let Some(builds) = &c.builds {
            let effective: Vec<&str> = builds
                .iter()
                .map(|b| b.binary.as_deref().unwrap_or(c.name.as_str()))
                .collect();
            let binaries: HashSet<&str> = effective.iter().copied().collect();
            for (idx, build) in builds.iter().enumerate() {
                let bin = effective[idx];
                if let Some(copy_from) = &build.copy_from
                    && !binaries.contains(copy_from.as_str())
                {
                    errors.push(format!(
                        "crate '{}': build binary '{}' has copy_from '{}' which is not a binary in this crate",
                        c.name, bin, copy_from
                    ));
                }
            }
        }
    }
}

/// Warn on unrecognized target triples in `defaults.targets` and per-build
/// `targets`.
fn check_target_triples(config: &Config, warnings: &mut Vec<String>) {
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
                "unrecognized target triple '{}' in {}",
                triple, context
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
                    let bin = b.binary.as_deref().unwrap_or(c.name.as_str());
                    for t in targets {
                        check_triple(t, &format!("crate '{}' build '{}'", c.name, bin));
                    }
                }
            }
        }
    }
}

/// Warn when changelog `skip:true` coexists with other configured fields,
/// and when `use:` has an unrecognized value.
fn check_changelog(config: &Config, warnings: &mut Vec<String>) {
    if let Some(cl) = &config.changelog
        && cl.skip == Some(anodizer_core::config::StringOrBool::Bool(true))
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
                "changelog.skip is true but other changelog fields are also set (they will be ignored)".to_string(),
            );
        }
    }

    if let Some(cl) = &config.changelog
        && let Some(ref use_source) = cl.use_source
        && use_source != "git"
        && use_source != "github-native"
    {
        warnings.push(format!(
            "unrecognized changelog 'use' value '{}' (valid: git, github-native)",
            use_source
        ));
    }
}

/// Warn when a recipient-visible announce template references a secret-named
/// `Env` variable (e.g. `{{ Env.GITHUB_TOKEN }}`).
///
/// Outbound redaction masks any secret-named env value before it reaches a
/// recipient (sent as the literal `$NAME`), so embedding such a reference in
/// the message/title/body a reader will see is almost always an authoring
/// mistake. Only content fields are scanned — routing fields (webhook URLs,
/// bot tokens, channel IDs, SMTP credentials) legitimately carry secrets and
/// are skipped to avoid noise. `reddit.url_template` is treated as content
/// because a token-named reference in a public link is a leak.
fn check_announce_secret_exposure(config: &Config, warnings: &mut Vec<String>) {
    let Some(announce) = &config.announce else {
        return;
    };

    let scan = |field: &str, value: &Option<String>, warnings: &mut Vec<String>| {
        if let Some(text) = value {
            warn_secret_env_refs(field, text, warnings);
        }
    };

    if let Some(b) = &announce.bluesky {
        scan(
            "announce.bluesky.message_template",
            &b.message_template,
            warnings,
        );
    }
    if let Some(d) = &announce.discourse {
        scan(
            "announce.discourse.title_template",
            &d.title_template,
            warnings,
        );
        scan(
            "announce.discourse.message_template",
            &d.message_template,
            warnings,
        );
    }
    if let Some(l) = &announce.linkedin {
        scan(
            "announce.linkedin.message_template",
            &l.message_template,
            warnings,
        );
    }
    if let Some(o) = &announce.opencollective {
        scan(
            "announce.opencollective.title_template",
            &o.title_template,
            warnings,
        );
        scan(
            "announce.opencollective.message_template",
            &o.message_template,
            warnings,
        );
    }
    if let Some(t) = &announce.twitter {
        scan(
            "announce.twitter.message_template",
            &t.message_template,
            warnings,
        );
    }
    if let Some(m) = &announce.mastodon {
        scan(
            "announce.mastodon.message_template",
            &m.message_template,
            warnings,
        );
    }
    if let Some(d) = &announce.discord {
        scan(
            "announce.discord.message_template",
            &d.message_template,
            warnings,
        );
        scan("announce.discord.author", &d.author, warnings);
    }
    if let Some(w) = &announce.webhook {
        scan(
            "announce.webhook.message_template",
            &w.message_template,
            warnings,
        );
    }
    if let Some(t) = &announce.telegram {
        scan(
            "announce.telegram.message_template",
            &t.message_template,
            warnings,
        );
    }
    if let Some(t) = &announce.teams {
        scan(
            "announce.teams.message_template",
            &t.message_template,
            warnings,
        );
        scan("announce.teams.title_template", &t.title_template, warnings);
    }
    if let Some(m) = &announce.mattermost {
        scan(
            "announce.mattermost.message_template",
            &m.message_template,
            warnings,
        );
        scan(
            "announce.mattermost.title_template",
            &m.title_template,
            warnings,
        );
    }
    if let Some(e) = &announce.email {
        scan(
            "announce.email.subject_template",
            &e.subject_template,
            warnings,
        );
        scan(
            "announce.email.message_template",
            &e.message_template,
            warnings,
        );
    }
    if let Some(r) = &announce.reddit {
        scan(
            "announce.reddit.title_template",
            &r.title_template,
            warnings,
        );
        scan("announce.reddit.url_template", &r.url_template, warnings);
    }
    if let Some(s) = &announce.slack {
        scan(
            "announce.slack.message_template",
            &s.message_template,
            warnings,
        );
        if let Some(blocks) = &s.blocks {
            for (i, block) in blocks.iter().enumerate() {
                if let Some(text) = &block.text {
                    warn_secret_env_refs(
                        &format!("announce.slack.blocks[{}].text", i),
                        &text.text,
                        warnings,
                    );
                }
            }
        }
        if let Some(attachments) = &s.attachments {
            for (i, att) in attachments.iter().enumerate() {
                let prefix = format!("announce.slack.attachments[{}]", i);
                scan(&format!("{}.text", prefix), &att.text, warnings);
                scan(&format!("{}.title", prefix), &att.title, warnings);
                scan(&format!("{}.fallback", prefix), &att.fallback, warnings);
                scan(&format!("{}.pretext", prefix), &att.pretext, warnings);
                scan(&format!("{}.footer", prefix), &att.footer, warnings);
            }
        }
    }
}

/// Push a warning for every `Env.<NAME>` reference inside a render block of
/// `text` whose `NAME` looks like a secret.
///
/// Only refs inside a `{{ ... }}` expression or a `{% ... %}` statement are
/// considered — bare prose like `set Env.GITHUB_TOKEN first` never renders
/// under Tera, so it cannot leak and must not warn. Each block span is
/// scanned independently with [`anodizer_core::template::ENV_REF_PATTERN`], so
/// multiple refs in one block (e.g. `{{ Env.A | default(Env.B_TOKEN) }}`) are
/// all caught. Both Tera (`Env.X`) and Go-style (`.Env.X`) forms match — the
/// capture starts after the dot, so a leading `.` is irrelevant.
fn warn_secret_env_refs(field: &str, text: &str, warnings: &mut Vec<String>) {
    static BLOCK_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Non-greedy inner captures so adjacent blocks stay separate spans.
        anodizer_core::util::static_regex(r"(?s)\{\{(.*?)\}\}|\{%(.*?)%\}")
    });
    static ENV_REF: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        anodizer_core::util::static_regex(anodizer_core::template::ENV_REF_PATTERN)
    });
    for block in BLOCK_RE.captures_iter(text) {
        // Exactly one alternation arm matches per block; take whichever did.
        let inner = block
            .get(1)
            .or_else(|| block.get(2))
            .map(|m| m.as_str())
            .unwrap_or("");
        for cap in ENV_REF.captures_iter(inner) {
            let name = &cap[1];
            let upper = name.to_uppercase();
            if anodizer_core::redact::SECRET_KEY_SUFFIXES
                .iter()
                .any(|suffix| upper.ends_with(suffix))
            {
                warnings.push(format!(
                    "{field} references secret-named var Env.{name}; its value is masked by outbound redaction (sent as \"${name}\"), so embedding it here is almost certainly a mistake — remove the reference"
                ));
            }
        }
    }
}

/// Warn when checksum `skip:true` coexists with other configured fields
/// (both `defaults.checksum` and per-crate `checksum`).
fn check_checksum_skip_conflicts(config: &Config, warnings: &mut Vec<String>) {
    if let Some(defaults) = &config.defaults
        && let Some(cksum) = &defaults.checksum
        && cksum.skip.as_ref().is_some_and(|d| d.as_bool())
    {
        let has_other = cksum.algorithm.is_some()
            || cksum.name_template.is_some()
            || cksum.extra_files.is_some()
            || cksum.ids.is_some();
        if has_other {
            warnings.push(
                "defaults.checksum.skip is true but other checksum fields are also set (they will be ignored)".to_string(),
            );
        }
    }

    for c in &config.crates {
        if let Some(cksum) = &c.checksum
            && cksum.skip.as_ref().is_some_and(|d| d.as_bool())
        {
            let has_other = cksum.algorithm.is_some()
                || cksum.name_template.is_some()
                || cksum.extra_files.is_some()
                || cksum.ids.is_some();
            if has_other {
                warnings.push(format!(
                    "checksum skip is true for crate '{}' but other checksum fields are also set (they will be ignored)",
                    c.name,
                ));
            }
        }
    }
}

/// Each non-empty crate `path` must point to an existing directory.
fn check_crate_paths(config: &Config, errors: &mut Vec<String>) {
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
}

/// Warn on unrecognized sign artifact filter values.
fn check_sign_artifact_filters(config: &Config, warnings: &mut Vec<String>) {
    let valid_artifact_filters = [
        "none", "all", "checksum", "source", "archive", "binary", "package",
    ];
    for sign_cfg in &config.signs {
        if let Some(ref filter) = sign_cfg.artifacts
            && !valid_artifact_filters.contains(&filter.as_str())
        {
            warnings.push(format!(
                "unrecognized signs artifacts filter '{}' (valid: {})",
                filter,
                valid_artifact_filters.join(", ")
            ));
        }
    }
}

/// Warn on unrecognized checksum algorithm values in `defaults.checksum`
/// and per-crate `checksum`.
fn check_checksum_algorithms(config: &Config, warnings: &mut Vec<String>) {
    let valid_algorithms = [
        "sha1", "sha224", "sha256", "sha384", "sha512", "blake2b", "blake2s",
    ];
    if let Some(defaults) = &config.defaults
        && let Some(cksum) = &defaults.checksum
        && let Some(ref algo) = cksum.algorithm
        && !valid_algorithms.contains(&algo.as_str())
    {
        warnings.push(format!(
            "unrecognized defaults.checksum algorithm '{}' (valid: {})",
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
                "unrecognized checksum algorithm '{1}' for crate '{0}' (valid: {2})",
                c.name,
                algo,
                valid_algorithms.join(", ")
            ));
        }
    }
}

/// `source.format` must be one of the supported archive formats.
fn check_source_format(config: &Config, errors: &mut Vec<String>) {
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
}

/// SBOM `artifacts` values must be from the allow-list.
fn check_sbom_configs(config: &Config, errors: &mut Vec<String>) {
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
}

/// Per-crate blob entries require a recognized `provider` and a non-empty
/// `bucket`.
fn check_blob_configs(config: &Config, errors: &mut Vec<String>) {
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
}

/// Environment / tool availability checks (warnings only). Probes
/// cross-compilation tools, docker + buildx, the GitHub token, nfpm, and
/// the signing binaries.
fn check_environment(config: &Config, warnings: &mut Vec<String>) {
    check_cross_tooling(config, warnings);
    check_docker_tooling(config, warnings);
    check_github_token(config, warnings);
    check_nfpm_tool(config, warnings);
    // Blob storage cloud credentials are validated at upload time by the
    // SDK — object_store handles S3/GCS/Azure natively. Config correctness
    // (provider, bucket) is already validated in check_blob_configs.
    check_signing_tools(config, warnings);
}

fn check_cross_tooling(config: &Config, warnings: &mut Vec<String>) {
    let needs_cross = config.crates.iter().any(|c| {
        use anodizer_core::config::CrossStrategy;
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
}

fn check_docker_tooling(config: &Config, warnings: &mut Vec<String>) {
    let needs_docker = config.crates.iter().any(|c| c.dockers_v2.is_some());
    if !needs_docker {
        return;
    }
    if !tool_available("docker") {
        warnings.push("docker is not installed but docker sections are configured".to_string());
        return;
    }
    // The buildx probe surfaces three states:
    //   Ok(true)  → buildx subcommand exists.
    //   Ok(false) → docker present but buildx subcommand missing.
    //   Err(_)    → spawn failed (typically: docker disappeared between
    //               the find_binary check above and now, e.g. PATH race).
    //               Collapse Err to "buildx unavailable" and trace-log the
    //               io::Error so verbose runs can see the underlying cause.
    let buildx_ok = match anodizer_core::docker_detect::buildx_available() {
        Ok(b) => b,
        Err(e) => {
            tracing::trace!(error = %e, "buildx probe failed");
            false
        }
    };
    if !buildx_ok {
        warnings
            .push("docker buildx is not available but docker sections are configured".to_string());
    }
}

fn check_github_token(config: &Config, warnings: &mut Vec<String>) {
    let needs_release = config.crates.iter().any(|c| c.release.is_some());
    if needs_release
        && std::env::var("ANODIZER_GITHUB_TOKEN").is_err()
        && std::env::var("GITHUB_TOKEN").is_err()
    {
        warnings.push(
            "no GitHub token found but release sections are configured; set GITHUB_TOKEN or ANODIZER_GITHUB_TOKEN"
                .to_string(),
        );
    }
}

fn check_nfpm_tool(config: &Config, warnings: &mut Vec<String>) {
    let needs_nfpm = config.crates.iter().any(|c| c.nfpms.is_some());
    if needs_nfpm && !tool_available("nfpm") {
        warnings.push("nfpm is not installed but nfpm sections are configured".to_string());
    }
}

fn check_signing_tools(config: &Config, warnings: &mut Vec<String>) {
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

/// Emit warnings, then either log success or emit all errors and bail.
fn report_results(log: &StageLogger, warnings: &[String], errors: &[String]) -> Result<()> {
    for w in warnings {
        log.warn(w);
    }

    if errors.is_empty() {
        log.status("Config is valid.");
        Ok(())
    } else {
        for e in errors {
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
    if !tag_template.is_empty() && !anodizer_core::git::has_version_placeholder(tag_template) {
        errors.push(format!(
            "{}: tag_template '{}' must contain '{{{{ .Version }}}}' or '{{{{ Version }}}}' \
             (e.g. 'v{{{{ .Version }}}}' or 'myapp-v{{{{ Version }}}}')",
            context, tag_template
        ));
    }
}

fn tool_available(name: &str) -> bool {
    anodizer_core::util::find_binary(name)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{Config, CrateConfig, WorkspaceConfig};

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
        use anodizer_core::config::BuildConfig;
        let mut c = make_crate("a", "a-v{{ .Version }}", None);
        c.builds = Some(vec![
            BuildConfig {
                binary: Some("a".to_string()),
                ..Default::default()
            },
            BuildConfig {
                binary: Some("b".to_string()),
                copy_from: Some("a".to_string()),
                ..Default::default()
            },
        ]);
        let config = make_config(vec![c]);
        assert!(run_checks(&config, false, &test_logger()).is_ok());
    }

    #[test]
    fn test_copy_from_invalid() {
        use anodizer_core::config::BuildConfig;
        let mut c = make_crate("a", "a-v{{ .Version }}", None);
        c.builds = Some(vec![BuildConfig {
            binary: Some("b".to_string()),
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
        use anodizer_core::config::{ChangelogConfig, ChangelogGroup};
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.changelog = Some(ChangelogConfig {
            skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
            sort: Some("desc".to_string()),
            header: Some(anodizer_core::config::ContentSource::Inline(
                "header".to_string(),
            )),
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
        use anodizer_core::config::{ChecksumConfig, Defaults, StringOrBool};
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.defaults = Some(Defaults {
            checksum: Some(ChecksumConfig {
                skip: Some(StringOrBool::Bool(true)),
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
        use anodizer_core::config::{ChecksumConfig, StringOrBool};
        let mut c = make_crate("a", "a-v{{ .Version }}", None);
        c.checksum = Some(ChecksumConfig {
            skip: Some(StringOrBool::Bool(true)),
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
    fn test_workspace_depends_on_cross_workspace_passes() {
        // A crate in one workspace can depend on a crate in another workspace.
        // The release engine topo-sorts across all workspaces, so the check
        // validator must not flag cross-workspace references as missing.
        let config = Config {
            project_name: "test".to_string(),
            workspaces: Some(vec![
                WorkspaceConfig {
                    name: "core-ws".to_string(),
                    crates: vec![make_crate("core", "core-v{{ .Version }}", None)],
                    ..Default::default()
                },
                WorkspaceConfig {
                    name: "app-ws".to_string(),
                    crates: vec![make_crate("app", "app-v{{ .Version }}", Some(vec!["core"]))],
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        assert!(
            run_checks(&config, false, &test_logger()).is_ok(),
            "cross-workspace depends_on should be accepted"
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
        use anodizer_core::config::SourceConfig;
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
        use anodizer_core::config::SourceConfig;
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
        use anodizer_core::config::SbomConfig;
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
        use anodizer_core::config::SbomConfig;
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
        use anodizer_core::config::BlobConfig;
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
        use anodizer_core::config::BlobConfig;
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
        use anodizer_core::config::BlobConfig;
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
        use anodizer_core::config::BlobConfig;
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
        use anodizer_core::config::BlobConfig;
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

    // -----------------------------------------------------------------------
    // Announce secret-exposure lint tests
    // -----------------------------------------------------------------------

    use anodizer_core::config::{
        AnnounceConfig, BlueskyAnnounce, DiscourseAnnounce, EmailAnnounce, SlackAnnounce,
        SlackAttachment, SlackBlock, SlackTextObject, TwitterAnnounce,
    };

    fn collect_announce_warnings(announce: AnnounceConfig) -> Vec<String> {
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.announce = Some(announce);
        let mut warnings = Vec::new();
        check_announce_secret_exposure(&config, &mut warnings);
        warnings
    }

    #[test]
    fn test_announce_secret_warns_on_token_in_message() {
        let warnings = collect_announce_warnings(AnnounceConfig {
            twitter: Some(TwitterAnnounce {
                message_template: Some("deploy {{ Env.GITHUB_TOKEN }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            warnings.len(),
            1,
            "expected one warning, got: {:?}",
            warnings
        );
        assert!(warnings[0].contains("announce.twitter.message_template"));
        assert!(warnings[0].contains("Env.GITHUB_TOKEN"));
        assert!(
            warnings[0].contains("$GITHUB_TOKEN"),
            "warning should state the masked form: {}",
            warnings[0]
        );
    }

    #[test]
    fn test_announce_secret_warns_on_title_and_email_subject() {
        let title_warnings = collect_announce_warnings(AnnounceConfig {
            discourse: Some(DiscourseAnnounce {
                title_template: Some("release {{ Env.SIGNING_KEY }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(title_warnings.len(), 1, "got: {:?}", title_warnings);
        assert!(title_warnings[0].contains("announce.discourse.title_template"));
        assert!(title_warnings[0].contains("Env.SIGNING_KEY"));

        let email_warnings = collect_announce_warnings(AnnounceConfig {
            email: Some(EmailAnnounce {
                subject_template: Some("v{{ Env.NPM_PASSWORD }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(email_warnings.len(), 1, "got: {:?}", email_warnings);
        assert!(email_warnings[0].contains("announce.email.subject_template"));
        assert!(email_warnings[0].contains("Env.NPM_PASSWORD"));
    }

    #[test]
    fn test_announce_secret_warns_on_go_style_dotted_env() {
        let warnings = collect_announce_warnings(AnnounceConfig {
            twitter: Some(TwitterAnnounce {
                message_template: Some("{{ .Env.CARGO_REGISTRY_TOKEN }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(warnings.len(), 1, "got: {:?}", warnings);
        assert!(warnings[0].contains("Env.CARGO_REGISTRY_TOKEN"));
    }

    #[test]
    fn test_announce_secret_warns_in_slack_blocks_and_attachments() {
        let warnings = collect_announce_warnings(AnnounceConfig {
            slack: Some(SlackAnnounce {
                blocks: Some(vec![SlackBlock {
                    block_type: "section".to_string(),
                    text: Some(SlackTextObject {
                        text_type: "mrkdwn".to_string(),
                        text: "see {{ Env.SLACK_API_TOKEN }}".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                attachments: Some(vec![SlackAttachment {
                    footer: Some("built by {{ Env.BUILDER_SECRET }}".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(warnings.len(), 2, "got: {:?}", warnings);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("announce.slack.blocks[0].text")
                    && w.contains("Env.SLACK_API_TOKEN")),
            "block-nested secret not warned: {:?}",
            warnings
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("announce.slack.attachments[0].footer")
                    && w.contains("Env.BUILDER_SECRET")),
            "attachment-nested secret not warned: {:?}",
            warnings
        );
    }

    #[test]
    fn test_announce_secret_silent_on_non_secret_refs() {
        // Non-secret placeholders, a non-secret env var, a provider with no
        // template, and an absent announce block all stay silent.
        let warnings = collect_announce_warnings(AnnounceConfig {
            bluesky: Some(BlueskyAnnounce {
                message_template: Some(
                    "{{ ProjectName }} {{ Tag }} home={{ Env.HOME }}".to_string(),
                ),
                ..Default::default()
            }),
            twitter: Some(TwitterAnnounce {
                message_template: None,
                ..Default::default()
            }),
            ..Default::default()
        });
        assert!(
            warnings.is_empty(),
            "non-secret refs should not warn: {:?}",
            warnings
        );

        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.announce = None;
        let mut no_announce = Vec::new();
        check_announce_secret_exposure(&config, &mut no_announce);
        assert!(
            no_announce.is_empty(),
            "absent announce block should not warn: {:?}",
            no_announce
        );
    }

    #[test]
    fn test_announce_secret_silent_on_bare_prose_no_braces() {
        // A secret-named ref in plain prose (outside any {{ }} / {% %} block)
        // never renders under Tera, so it cannot leak and must stay silent.
        let warnings = collect_announce_warnings(AnnounceConfig {
            twitter: Some(TwitterAnnounce {
                message_template: Some("contact Env.GITHUB_TOKEN admin".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert!(
            warnings.is_empty(),
            "bare-prose Env ref outside a render block should not warn: {:?}",
            warnings
        );
    }

    #[test]
    fn test_announce_secret_warns_on_both_refs_in_one_block() {
        // Two Env refs inside ONE render block must both be flagged; only the
        // secret-named one(s) warn (PROJECT is not secret, B_TOKEN is).
        let warnings = collect_announce_warnings(AnnounceConfig {
            twitter: Some(TwitterAnnounce {
                message_template: Some("{{ Env.A_TOKEN | default(Env.B_TOKEN) }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            warnings.len(),
            2,
            "both secret refs should warn: {:?}",
            warnings
        );
        assert!(
            warnings.iter().any(|w| w.contains("Env.A_TOKEN")),
            "first ref missed: {:?}",
            warnings
        );
        assert!(
            warnings.iter().any(|w| w.contains("Env.B_TOKEN")),
            "second ref in same block missed: {:?}",
            warnings
        );
    }
}
