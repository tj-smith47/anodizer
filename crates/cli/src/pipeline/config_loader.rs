//! Config discovery, format detection, and `includes:` resolution.
//!
//! Hosts `find_config` / `load_config` plus the recursive include walker
//! (file + URL fetch, cycle detection, deep-merge) and the post-load
//! normalization passes (commit-author defaults). The monorepo path-prefix
//! pass lives in the sibling [`super::monorepo`] module and is invoked from
//! `load_config`.

use anodizer_core::config::{Config, IncludeSpec};
use anodizer_core::env_expand::expand_env as expand_env_vars;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::monorepo::apply_monorepo_defaults;

/// Cap on recursion depth for `includes:` chains. The format imposes no
/// inherent limit; 32 sits well above any plausible org-shared fan-out
/// while staying clear of the main thread's default stack budget, on
/// which this include walker recurses synchronously.
const MAX_INCLUDE_DEPTH: usize = 32;

/// Find config file. If `config_override` is provided, use that path directly;
/// otherwise search the current directory for well-known config file names.
///
/// When the Cargo.toml fallback fires, the warning is routed through
/// `log.warn` if a logger is supplied; otherwise it falls back to
/// `tracing::warn!` so the bare-pipeline / tests path still surfaces the
/// signal without bypassing structured logging.
pub fn find_config(config_override: Option<&Path>) -> Result<PathBuf> {
    find_config_with_logger(config_override, None)
}

/// Variant of [`find_config`] that routes the Cargo.toml-fallback
/// warning through the caller's `StageLogger` so the message appears
/// with the same stage prefix as the rest of the command's output.
pub fn find_config_with_logger(
    config_override: Option<&Path>,
    log: Option<&StageLogger>,
) -> Result<PathBuf> {
    if let Some(path) = config_override {
        if path.exists() {
            return Ok(path.to_path_buf());
        }
        bail!("config file not found: {}", path.display());
    }
    let candidates = [
        ".anodizer.yaml",
        ".anodizer.yml",
        ".anodizer.toml",
        "anodizer.yaml",
        "anodizer.yml",
        "anodizer.toml",
    ];
    for name in &candidates {
        let path = PathBuf::from(name);
        if path.exists() {
            return Ok(path);
        }
    }
    // Fallback: if Cargo.toml exists, use a default config instead of erroring.
    if Path::new("Cargo.toml").exists() {
        let msg = "no anodizer config found; using defaults from Cargo.toml";
        match log {
            Some(l) => l.warn(msg),
            None => tracing::warn!("{}", msg),
        }
        return Ok(PathBuf::from("Cargo.toml"));
    }
    bail!(
        "no anodizer config file found (tried: {}). Run `anodizer init` to generate one.",
        candidates.join(", ")
    )
}

/// Find an anodizer config by searching `base` for the well-known config
/// file names, without mutating the process-global cwd.
///
/// [`find_config`] resolves candidates relative to the current directory,
/// so callers that want to probe a *different* directory must `set_current_dir`
/// around the call — a process-global mutation that is fragile under any
/// concurrency and easy to leak on an early return. This variant joins each
/// candidate against `base` instead. Returns the matched path (joined under
/// `base`, including the `Cargo.toml` fallback, so [`load_config`] still
/// recognizes the fallback by filename). Best-effort callers (allow-list /
/// hint derivation) can `.ok()` the result.
pub fn find_config_in(base: &Path) -> Result<PathBuf> {
    let candidates = [
        ".anodizer.yaml",
        ".anodizer.yml",
        ".anodizer.toml",
        "anodizer.yaml",
        "anodizer.yml",
        "anodizer.toml",
    ];
    for name in &candidates {
        let path = base.join(name);
        if path.exists() {
            return Ok(path);
        }
    }
    let cargo_toml = base.join("Cargo.toml");
    if cargo_toml.exists() {
        return Ok(cargo_toml);
    }
    bail!(
        "no anodizer config file found under {} (tried: {}). Run `anodizer init` to generate one.",
        base.display(),
        candidates.join(", ")
    )
}

/// Find + load the anodizer config rooted at `base` in one call, without
/// mutating the process-global cwd. Thin combinator over [`find_config_in`]
/// and [`load_config`] for the best-effort config probes (signature
/// allow-list, docker-backend hint, all-prebuilt short-circuit) that each
/// previously open-coded a cwd-save / find / load / cwd-restore block.
pub fn load_repo_config(base: &Path) -> Result<Config> {
    let path = find_config_in(base)?;
    load_config(&path)
}

/// Deep-merge `overlay` into `base`. Mappings are merged recursively,
/// sequences are concatenated, and scalars/other values are replaced.
fn merge_yaml(base: &mut serde_yaml_ng::Value, overlay: &serde_yaml_ng::Value) {
    match (base, overlay) {
        (serde_yaml_ng::Value::Mapping(base_map), serde_yaml_ng::Value::Mapping(overlay_map)) => {
            for (key, value) in overlay_map {
                match base_map.get_mut(key) {
                    Some(existing) => merge_yaml(existing, value),
                    None => {
                        base_map.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (serde_yaml_ng::Value::Sequence(base_seq), serde_yaml_ng::Value::Sequence(overlay_seq)) => {
            base_seq.extend(overlay_seq.iter().cloned());
        }
        (base_val, overlay_val) => {
            *base_val = overlay_val.clone();
        }
    }
}

/// Load config from a file, auto-detecting format by extension.
///
/// For YAML files, processes `includes` by deep-merging included files together as
/// defaults, then merging the base (local) config on top. This means the base config
/// always takes priority over values from included files — includes provide defaults,
/// not overrides.
pub fn load_config(path: &Path) -> Result<Config> {
    // Special case: Cargo.toml fallback returns a default Config. The
    // find_config function returns "Cargo.toml" when no anodizer config file
    // exists but a Cargo.toml is present in the working directory.
    if path.file_name().and_then(|n| n.to_str()) == Some("Cargo.toml") {
        return Ok(Config::default());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    // Walk the raw YAML pre-parse for two checks that lose information
    // once typed deserialization runs:
    //   * legacy `snapshot.name_template` (renamed to `version_template`
    //     historically; serde alias accepts both but collapses them on parse).
    //   * legacy V1 `dockers:` block — anodizer is V2-only by design;
    //     without this check `deny_unknown_fields` emits a generic
    //     "unknown field" error that does not point at `docker_v2:`.
    // Best-effort — YAML parse failures are reported by the typed loader below.
    if (ext == "yaml" || ext == "yml")
        && let Ok(raw) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&content)
    {
        anodizer_core::config::warn_on_legacy_snapshot_name_template(&raw);
        anodizer_core::config::warn_on_legacy_furies_alias(&raw);
        anodizer_core::config::warn_on_legacy_nfpm_builds(&raw);
        anodizer_core::config::warn_on_legacy_disable_alias(&raw);
        anodizer_core::config::validate_no_docker_v1(&raw).map_err(anyhow::Error::msg)?;
        anodizer_core::config::validate_no_mcp_github(&raw).map_err(anyhow::Error::msg)?;
    }

    let mut config = match ext {
        "yaml" | "yml" => load_yaml_config_with_includes(path, &content)?,
        "toml" => load_toml_config_with_includes(path, &content)?,
        _ => bail!("unsupported config format: {}", ext),
    };

    // Fold deprecated archive `format` / `format_overrides[].format` /
    // `builds` aliases into their canonical fields, emitting deprecation
    // warnings. Done before validation so unique-id checks see the
    // post-fold state.
    anodizer_core::config::apply_archive_legacy_aliases(&mut config);
    // Fold the singular `binary:` cask field into the canonical `binaries:`
    // list (deprecated rename) and emit a deprecation warning per occurrence.
    anodizer_core::config::apply_homebrew_cask_legacy_singulars(&mut config);

    // Validate config schema version
    anodizer_core::config::validate_version(&config).map_err(anyhow::Error::msg)?;
    // Validate git.tag_sort if present
    anodizer_core::config::validate_tag_sort(&config).map_err(anyhow::Error::msg)?;
    // Validate partial.by ("os" | "target") before either target-resolution
    // path reads it (one rejects unknowns, the other silently mis-groups).
    anodizer_core::config::validate_partial(&config).map_err(anyhow::Error::msg)?;
    // Validate archives[].format_overrides[].os
    anodizer_core::config::validate_format_overrides(&config).map_err(anyhow::Error::msg)?;
    // Validate release block does not configure multiple SCM backends.
    anodizer_core::config::validate_release_backends(&config).map_err(anyhow::Error::msg)?;
    // Validate nightly.publish_repo is "owner/repo" shaped (fail at config
    // time rather than as a confusing 404 when the release is created).
    anodizer_core::config::validate_nightly_publish_repo(&config).map_err(anyhow::Error::msg)?;
    // Validate defaults.crates / defaults.workspaces axis matches top-level.
    anodizer_core::config::validate_defaults_axis(&config).map_err(anyhow::Error::msg)?;
    // Validate homebrew_cask does not set both url_template and url.template.
    anodizer_core::config::validate_homebrew_cask_url_template(&config)
        .map_err(anyhow::Error::msg)?;
    // Validate archives[].id and universal_binaries[].id uniqueness.
    anodizer_core::config::validate_id_uniqueness(&config).map_err(anyhow::Error::msg)?;
    // Validate `builder: prebuilt` builds carry a `prebuilt.path`,
    // explicit targets, and no cargo-only knobs.
    anodizer_core::config::validate_builds(&config).map_err(anyhow::Error::msg)?;
    // Validate changelog.groups subgroup depth (capped at one level).
    anodizer_core::config::validate_changelog_groups_depth(&config).map_err(anyhow::Error::msg)?;
    // Validate changelog.paths[] syntax (reject leading `/` and empty entries).
    anodizer_core::config::validate_changelog_paths(&config).map_err(anyhow::Error::msg)?;
    anodizer_core::config::warn_on_submitter_required(&config);
    anodizer_core::config::warn_on_legacy_homebrew_formula(&config);
    // The deprecated nested `dockers_v2[].retry:` / `docker_manifests[].retry:`
    // in favour of the top-level `retry:` block.
    anodizer_core::config::warn_on_legacy_docker_retry(&config);

    // source.prefix_template defaults to source.name_template when unset
    // (matches the long-documented behavior — see SourceConfig docs).
    // Applied at config-load so every downstream stage reading prefix_template
    // sees the resolved value.
    if let Some(ref mut src) = config.source {
        src.apply_prefix_template_default();
    }

    // Apply monorepo defaults: when monorepo.dir is set and a crate's path
    // is empty or ".", default it to monorepo.dir.
    apply_monorepo_defaults(&mut config);

    // Normalize commit_author defaults on every publisher config that carries
    // one. Fills in anodizer defaults
    // for empty name/email so error messages referencing author identity at
    // config-validation time see non-empty strings.
    normalize_commit_author_defaults(&mut config);

    // Fold workspace-level `defaults` into every per-crate config so
    // downstream stages can read from `crate_cfg.<field>` regardless of
    // whether the value was set per-crate or hoisted to defaults.
    anodizer_core::defaults_merge::apply_defaults(&mut config);

    // Derive per-crate publisher metadata (description / license / homepage /
    // authors) from each crate's `Cargo.toml [package]` so a plain Rust
    // project's publishers (winget/snapcraft/nfpm/homebrew/nix/...) resolve
    // these fields without a top-level `metadata:` YAML block. Runs after
    // `apply_monorepo_defaults` so each crate's `path` is fully resolved; the
    // crate dirs are read relative to the working directory (matching how the
    // build/binstall stages read `<crate.path>/Cargo.toml`).
    config.populate_derived_metadata(Path::new("."));

    Ok(config)
}

/// Walk the loaded config and fill in commit_author defaults on every
/// publisher that has one (homebrew formula + cask, scoop, chocolatey, winget,
/// nix, aur, krew). This is the per-publisher defaulting
/// pass; anodizer centralises here so the normalization runs once at load.
fn normalize_commit_author_defaults(config: &mut anodizer_core::config::Config) {
    for crate_cfg in &mut config.crates {
        normalize_crate_commit_author(crate_cfg);
    }
    if let Some(ws_list) = config.workspaces.as_mut() {
        for ws in ws_list {
            for crate_cfg in &mut ws.crates {
                normalize_crate_commit_author(crate_cfg);
            }
        }
    }
}

fn normalize_crate_commit_author(crate_cfg: &mut anodizer_core::config::CrateConfig) {
    let Some(ref mut pub_cfg) = crate_cfg.publish else {
        return;
    };
    if let Some(ref mut e) = pub_cfg.homebrew
        && let Some(ref mut ca) = e.commit_author
    {
        ca.normalize_defaults();
    }
    if let Some(ref mut e) = pub_cfg.scoop
        && let Some(ref mut ca) = e.commit_author
    {
        ca.normalize_defaults();
    }
    // Chocolatey has no commit_author (upstream publishes directly to Chocolatey's
    // feed API — no tap/repo commit happens).
    if let Some(ref mut e) = pub_cfg.winget
        && let Some(ref mut ca) = e.commit_author
    {
        ca.normalize_defaults();
    }
    if let Some(ref mut e) = pub_cfg.nix
        && let Some(ref mut ca) = e.commit_author
    {
        ca.normalize_defaults();
    }
    if let Some(ref mut e) = pub_cfg.aur
        && let Some(ref mut ca) = e.commit_author
    {
        ca.normalize_defaults();
    }
    if let Some(ref mut e) = pub_cfg.krew
        && let Some(ref mut ca) = e.commit_author
    {
        ca.normalize_defaults();
    }
}

/// Load a YAML config, processing `includes` by deep-merging included files
/// as defaults and then merging the base (local) config on top.
///
/// Include entries can be:
/// - Plain strings (file paths, backward compatible)
/// - `from_file:` mappings with a `path` key
/// - `from_url:` mappings with a `url` key and optional `headers`
fn load_yaml_config_with_includes(path: &Path, content: &str) -> Result<Config> {
    let base: serde_yaml_ng::Value = serde_yaml_ng::from_str(content)
        .with_context(|| format!("failed to parse YAML config: {}", path.display()))?;

    let include_entries: Vec<serde_yaml_ng::Value> = base
        .get("includes")
        .and_then(|v| v.as_sequence())
        .cloned()
        .unwrap_or_default();

    // Accumulate all included files into a merged defaults value.
    // The base config is then merged on top so its values always win.
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut visited: HashSet<String> = HashSet::new();
    // Mark the base config itself as visited so a child include cannot
    // form an A -> B -> A cycle back through the root. The same key is
    // also passed as `root_key` so a direct A -> A self-cycle errors
    // explicitly instead of silently deduping into an empty mapping.
    let root_key = canonical_path_key(path);
    if let Some(ref key) = root_key {
        visited.insert(key.clone());
    }
    let mut merged = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
    for entry in &include_entries {
        let overlay =
            resolve_include_recursive(entry, base_dir, path, &mut visited, 0, root_key.as_deref())?;
        merge_yaml(&mut merged, &overlay);
    }
    // Merge base config on top of the accumulated defaults (base wins).
    merge_yaml(&mut merged, &base);

    // Run the full-Config deserialize on a generously-sized worker thread so
    // hosts with a small main-thread stack reservation (Windows: 1 MiB)
    // cannot overflow inside serde's monomorphised visitor for the 60+
    // field Config struct.
    let path_display = path.display().to_string();
    anodizer_core::config::deserialize_on_worker(move || {
        serde_yaml_ng::from_value::<Config>(merged)
            .with_context(|| format!("failed to deserialize config: {}", path_display))
    })
}

/// Load a TOML config, processing `includes` using the same merge strategy
/// as YAML: included files provide defaults, the base config wins.
///
/// TOML includes support the same forms as YAML (plain strings, from_file,
/// from_url). The entries are converted to YAML values for processing by
/// `resolve_include`.
fn load_toml_config_with_includes(path: &Path, content: &str) -> Result<Config> {
    // Parse the base TOML to a generic toml::Value first to extract includes.
    let base_toml: toml::Value = toml::from_str(content)
        .with_context(|| format!("failed to parse TOML config: {}", path.display()))?;

    let include_entries: Vec<toml::Value> = base_toml
        .get("includes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if include_entries.is_empty() {
        // No includes — fast path: deserialize directly from TOML on a
        // worker thread so the host's main-thread stack reservation cannot
        // bound serde's monomorphised visitor for the giant `Config` struct.
        let path_display = path.display().to_string();
        let content_owned = content.to_string();
        return anodizer_core::config::deserialize_on_worker(move || {
            toml::from_str::<Config>(&content_owned)
                .with_context(|| format!("failed to deserialize TOML config: {}", path_display))
        });
    }

    // Convert the base TOML to a YAML Value so we can use the existing
    // deep-merge logic. Round-trip through serde_json::Value as an
    // intermediate format that both serde_yaml_ng and toml support.
    let base_json = serde_json::to_value(&base_toml)
        .with_context(|| "failed to convert TOML config to JSON for merging")?;
    let base_yaml: serde_yaml_ng::Value = serde_yaml_ng::to_value(&base_json)
        .with_context(|| "failed to convert TOML config to YAML for merging")?;

    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut visited: HashSet<String> = HashSet::new();
    let root_key = canonical_path_key(path);
    if let Some(ref key) = root_key {
        visited.insert(key.clone());
    }
    let mut merged = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
    for entry in &include_entries {
        // Convert each TOML include entry to a YAML value so resolve_include can handle it.
        let json_entry = serde_json::to_value(entry)
            .with_context(|| "failed to convert TOML include entry to JSON")?;
        let yaml_entry: serde_yaml_ng::Value = serde_yaml_ng::to_value(&json_entry)
            .with_context(|| "failed to convert TOML include entry to YAML")?;
        let overlay = resolve_include_recursive(
            &yaml_entry,
            base_dir,
            path,
            &mut visited,
            0,
            root_key.as_deref(),
        )?;
        merge_yaml(&mut merged, &overlay);
    }
    // Merge base config on top of the accumulated defaults (base wins).
    merge_yaml(&mut merged, &base_yaml);

    let path_display = path.display().to_string();
    anodizer_core::config::deserialize_on_worker(move || {
        serde_yaml_ng::from_value::<Config>(merged)
            .with_context(|| format!("failed to deserialize config: {}", path_display))
    })
}

/// Normalize a URL for include fetching.
///
/// If the URL does not start with `http://` or `https://`, prepend
/// `https://raw.githubusercontent.com/` (GitHub raw content shorthand,
/// the merge-mode behavior).
fn normalize_include_url(url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("https://raw.githubusercontent.com/{}", url)
    }
}

/// Maximum response body size for URL-fetched config files (10 MB).
const MAX_INCLUDE_BODY_SIZE: u64 = 10 * 1024 * 1024;

/// Fetch config content from a URL with optional headers, parsing as YAML or TOML
/// based on the URL file extension.
///
/// NOTE: reqwest is already a transitive dependency through stage-release, stage-announce,
/// and stage-blob. Since the CLI depends on all these crates, gating reqwest behind a
/// feature flag provides no practical binary size savings.
fn fetch_url_as_yaml(
    url: &str,
    headers: Option<&std::collections::HashMap<String, String>>,
    config_path: &Path,
) -> Result<serde_yaml_ng::Value> {
    let client = anodizer_core::http::blocking_client(Duration::from_secs(30))
        .with_context(|| "failed to build HTTP client for include URL fetch")?;

    let mut request = client.get(url);
    if let Some(hdrs) = headers {
        for (key, value) in hdrs {
            let expanded = expand_env_vars(value);
            request = request.header(key.as_str(), expanded);
        }
    }

    let response = request.send().with_context(|| {
        format!(
            "failed to fetch include URL '{}' (referenced from {})",
            url,
            config_path.display()
        )
    })?;

    if !response.status().is_success() {
        bail!(
            "include URL '{}' returned HTTP {} (referenced from {})",
            url,
            response.status(),
            config_path.display()
        );
    }

    // Check Content-Length header if available to reject obviously oversized responses.
    if let Some(content_length) = response.content_length()
        && content_length > MAX_INCLUDE_BODY_SIZE
    {
        bail!(
            "include URL '{}' response too large ({} bytes, max {} bytes) (referenced from {})",
            url,
            content_length,
            MAX_INCLUDE_BODY_SIZE,
            config_path.display()
        );
    }

    let body = response.text().with_context(|| {
        format!(
            "failed to read response body from include URL '{}' (referenced from {})",
            url,
            config_path.display()
        )
    })?;

    // Enforce body size limit after reading (Content-Length may be absent or inaccurate).
    if body.len() as u64 > MAX_INCLUDE_BODY_SIZE {
        bail!(
            "include URL '{}' response too large ({} bytes, max {} bytes) (referenced from {})",
            url,
            body.len(),
            MAX_INCLUDE_BODY_SIZE,
            config_path.display()
        );
    }

    // Detect format from URL path extension: if .toml, parse as TOML and convert to YAML.
    let is_toml = url
        .split('?')
        .next()
        .and_then(|path| path.rsplit('.').next())
        .map(|ext| ext.eq_ignore_ascii_case("toml"))
        .unwrap_or(false);

    if is_toml {
        let toml_val: toml::Value = toml::from_str(&body).with_context(|| {
            format!(
                "failed to parse TOML from include URL '{}' (referenced from {})",
                url,
                config_path.display()
            )
        })?;
        let json_val = serde_json::to_value(&toml_val).with_context(|| {
            format!(
                "failed to convert TOML to JSON from include URL '{}' (referenced from {})",
                url,
                config_path.display()
            )
        })?;
        serde_yaml_ng::to_value(&json_val).with_context(|| {
            format!(
                "failed to convert TOML to YAML from include URL '{}' (referenced from {})",
                url,
                config_path.display()
            )
        })
    } else {
        serde_yaml_ng::from_str(&body).with_context(|| {
            format!(
                "failed to parse YAML from include URL '{}' (referenced from {})",
                url,
                config_path.display()
            )
        })
    }
}

/// Canonicalize a path for cycle-detection / dedup. Falls back to the
/// raw path string when canonicalization fails (file missing, permission
/// denied) — those callers will hit a clearer downstream error.
fn canonical_path_key(path: &Path) -> Option<String> {
    match std::fs::canonicalize(path) {
        Ok(p) => Some(p.to_string_lossy().to_string()),
        Err(_) => path.to_str().map(|s| s.to_string()),
    }
}

/// Expand a leading `~` into the user's home directory and `$VAR` /
/// `${VAR}` references via [`expand_env_vars`].
///
/// `~` is rewritten only when it appears at the very start of the
/// rendered string AND is followed by `/` (or end-of-string), mirroring
/// the POSIX-shell word-initial tilde rule; anywhere else the literal
/// `~` is preserved so a config path like `./safe~backup.yaml` survives.
///
/// `~user/...` (POSIX user-home form) is NOT supported — only `~/` and
/// `$VAR` are recognized. A path like `~bob/foo` is returned unchanged
/// because resolving an arbitrary user's home requires a `getpwnam(3)`
/// call (or platform equivalent) which we deliberately avoid for the
/// security and cross-platform-portability cost.
fn expand_path_tilde_and_env(path_str: &str) -> String {
    let expanded = expand_env_vars(path_str);
    if let Some(rest) = expanded.strip_prefix('~')
        && let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty())
    {
        let home = PathBuf::from(home);
        let rest_trimmed = rest.strip_prefix('/').unwrap_or(rest);
        if rest.starts_with('/') || rest.is_empty() {
            return home.join(rest_trimmed).to_string_lossy().to_string();
        }
    }
    expanded
}

/// Resolve a single include entry recursively, walking the included
/// file's own `includes:` array (depth-first) before applying it to the
/// caller's merge tree.
///
/// Cycle detection: each include's canonical path (or normalized URL)
/// goes into `visited` before recursing; a repeat hit bails with the
/// chain so a misconfigured `A -> B -> A` surfaces with a clear
/// message. The same set survives across siblings, which means an
/// include referenced twice in a chain (or twice in the same array) is
/// deduplicated — the second hit returns an empty mapping. This matches
/// a "load once" expectation; users wanting an include's
/// values applied twice cannot express that anyway because the deep
/// merge is idempotent.
///
/// Path resolution: file includes inside a child config resolve
/// relative to THAT child's directory, not the root config's directory,
/// so a shared `includes/team-defaults.yaml` that itself references
/// `./platform.yaml` finds `includes/platform.yaml` correctly.
fn resolve_include_recursive(
    entry: &serde_yaml_ng::Value,
    base_dir: &Path,
    config_path: &Path,
    visited: &mut HashSet<String>,
    depth: usize,
    root_key: Option<&str>,
) -> Result<serde_yaml_ng::Value> {
    if depth >= MAX_INCLUDE_DEPTH {
        bail!(
            "includes: depth limit ({}) exceeded (referenced from {})",
            MAX_INCLUDE_DEPTH,
            config_path.display(),
        );
    }
    let spec: IncludeSpec = serde_yaml_ng::from_value(entry.clone())
        .with_context(|| format!("includes: invalid entry in {}", config_path.display()))?;

    // Resolve to (canonical key, raw YAML value, child base_dir, child config_path)
    let (key, mut value, child_base_dir, child_config_path) = match spec {
        IncludeSpec::Path(path_str) => {
            resolve_file_include_value(&path_str, base_dir, config_path)?
        }
        IncludeSpec::FromFile { from_file } => {
            resolve_file_include_value(&from_file.path, base_dir, config_path)?
        }
        IncludeSpec::FromUrl { from_url } => {
            let url = expand_env_vars(&normalize_include_url(&from_url.url));
            let value = fetch_url_as_yaml(&url, from_url.headers.as_ref(), config_path)?;
            // URL includes have no on-disk base_dir; child file includes
            // are resolved relative to the ORIGINAL config_path's parent,
            // which is the closest analogue. URL-to-URL includes resolve
            // by absolute URL anyway, so the base_dir is only consulted
            // if a URL include carries a relative file include — an
            // unusual mix that gets the same treatment as before.
            let child_base_dir = base_dir.to_path_buf();
            let child_config_path = PathBuf::from(&url);
            (url, value, child_base_dir, child_config_path)
        }
    };

    // Self-cycle: the included file resolves back to the root config (A
    // includes A, directly or transitively). Without this branch, the
    // root key pre-inserted into `visited` would silently dedup the
    // include into an empty mapping and the user would see no error for
    // a clearly malformed config.
    if let Some(rk) = root_key
        && key == rk
    {
        bail!(
            "includes: self-cycle detected at '{}' (referenced from {})",
            key,
            config_path.display(),
        );
    }

    // Dedup / cycle detection. A repeat hit returns an empty mapping so
    // sibling includes can keep accumulating without double-merging the
    // already-loaded values.
    if !visited.insert(key.clone()) {
        if depth > 0 {
            // Mid-chain repeat: this is a cycle, not just a dedup hit.
            // The same key can only re-appear in `visited` here because
            // an ancestor loaded it; report the cycle.
            bail!(
                "includes: cycle detected at '{}' (referenced from {})",
                key,
                config_path.display(),
            );
        }
        // Top-level dedup: an earlier sibling already loaded this
        // include. Return empty so the merge is a no-op.
        return Ok(serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new()));
    }

    // Strip `includes:` from the included value BEFORE returning so
    // typed deserialization at the end of `load_yaml_config_with_includes`
    // doesn't see a stale list. Recurse into it first.
    let child_entries: Vec<serde_yaml_ng::Value> = match &mut value {
        serde_yaml_ng::Value::Mapping(map) => map
            .remove("includes")
            .and_then(|v| match v {
                serde_yaml_ng::Value::Sequence(seq) => Some(seq),
                _ => None,
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };

    let mut accumulated = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
    for child_entry in &child_entries {
        let child_overlay = resolve_include_recursive(
            child_entry,
            &child_base_dir,
            &child_config_path,
            visited,
            depth + 1,
            root_key,
        )?;
        merge_yaml(&mut accumulated, &child_overlay);
    }
    // The included file's own contents apply ON TOP of its transitive
    // children — same "later wins" semantics the top-level loop uses.
    merge_yaml(&mut accumulated, &value);
    Ok(accumulated)
}

/// Read a file include from disk and return the canonical key, parsed
/// YAML value, the directory child includes resolve against, and the
/// child's display path (for error messages and cycle reporting).
fn resolve_file_include_value(
    path_str: &str,
    base_dir: &Path,
    config_path: &Path,
) -> Result<(String, serde_yaml_ng::Value, PathBuf, PathBuf)> {
    let expanded = expand_path_tilde_and_env(path_str);
    let include_path = if Path::new(&expanded).is_absolute() {
        // Absolute paths are still rejected for plain-string / from_file
        // entries — but only AFTER expansion, so a config that ships
        // `~/.config/anodize/defaults.yaml` is treated as the resolved
        // absolute home path and rejected with an actionable error.
        bail!(
            "includes: absolute paths are not allowed (got '{}' in {})",
            path_str,
            config_path.display()
        );
    } else {
        base_dir.join(&expanded)
    };
    let include_content = std::fs::read_to_string(&include_path).with_context(|| {
        format!(
            "failed to read include file '{}' (referenced from {})",
            include_path.display(),
            config_path.display()
        )
    })?;
    let value = load_include_as_yaml(&include_path, &include_content)?;
    let key =
        canonical_path_key(&include_path).unwrap_or_else(|| include_path.display().to_string());
    let child_base_dir = include_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok((key, value, child_base_dir, include_path))
}

/// Parse an include file as a serde_yaml_ng::Value, auto-detecting format
/// by extension (YAML or TOML).
fn load_include_as_yaml(
    include_path: &Path,
    include_content: &str,
) -> Result<serde_yaml_ng::Value> {
    let ext = include_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "toml" => {
            let toml_val: toml::Value = toml::from_str(include_content).with_context(|| {
                format!("failed to parse include file: {}", include_path.display())
            })?;
            let json_val = serde_json::to_value(&toml_val).with_context(|| {
                format!(
                    "failed to convert TOML include to JSON: {}",
                    include_path.display()
                )
            })?;
            serde_yaml_ng::to_value(&json_val).with_context(|| {
                format!(
                    "failed to convert TOML include to YAML: {}",
                    include_path.display()
                )
            })
        }
        _ => {
            // Default: parse as YAML (works for .yaml, .yml, and extensionless)
            serde_yaml_ng::from_str(include_content).with_context(|| {
                format!("failed to parse include file: {}", include_path.display())
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_find_config_with_override_existing() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("custom-config.yaml");
        fs::write(&cfg_path, "project_name: test\ncrates: []\n").unwrap();

        let result = find_config(Some(cfg_path.as_path()));
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert_eq!(result.unwrap(), cfg_path);
    }

    #[test]
    fn test_find_config_with_override_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("does-not-exist.yaml");

        let result = find_config(Some(cfg_path.as_path()));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("config file not found"),
            "unexpected error message: {}",
            msg
        );
    }

    #[test]
    fn test_find_config_override_with_subdirectory_path() {
        let tmp = TempDir::new().unwrap();
        let subdir = tmp.path().join("nested").join("dir");
        fs::create_dir_all(&subdir).unwrap();
        let cfg_path = subdir.join("my-release.toml");
        fs::write(&cfg_path, "project_name = \"test\"\ncrates = []\n").unwrap();

        let result = find_config(Some(cfg_path.as_path()));
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert_eq!(result.unwrap(), cfg_path);
    }

    // -----------------------------------------------------------------------
    // merge_yaml tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_merge_yaml_mappings_recursive() {
        let mut base: serde_yaml_ng::Value = serde_yaml_ng::from_str("a: 1\nb: 2").unwrap();
        let overlay: serde_yaml_ng::Value = serde_yaml_ng::from_str("b: 99\nc: 3").unwrap();
        merge_yaml(&mut base, &overlay);
        assert_eq!(base["a"], serde_yaml_ng::Value::Number(1.into()));
        assert_eq!(base["b"], serde_yaml_ng::Value::Number(99.into()));
        assert_eq!(base["c"], serde_yaml_ng::Value::Number(3.into()));
    }

    #[test]
    fn test_merge_yaml_nested_mappings() {
        let mut base: serde_yaml_ng::Value =
            serde_yaml_ng::from_str("outer:\n  x: 1\n  y: 2").unwrap();
        let overlay: serde_yaml_ng::Value =
            serde_yaml_ng::from_str("outer:\n  y: 99\n  z: 3").unwrap();
        merge_yaml(&mut base, &overlay);
        assert_eq!(base["outer"]["x"], serde_yaml_ng::Value::Number(1.into()));
        assert_eq!(base["outer"]["y"], serde_yaml_ng::Value::Number(99.into()));
        assert_eq!(base["outer"]["z"], serde_yaml_ng::Value::Number(3.into()));
    }

    #[test]
    fn test_merge_yaml_sequences_concatenate() {
        let mut base: serde_yaml_ng::Value =
            serde_yaml_ng::from_str("items:\n  - a\n  - b").unwrap();
        let overlay: serde_yaml_ng::Value =
            serde_yaml_ng::from_str("items:\n  - c\n  - d").unwrap();
        merge_yaml(&mut base, &overlay);
        let items = base["items"].as_sequence().unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].as_str().unwrap(), "a");
        assert_eq!(items[1].as_str().unwrap(), "b");
        assert_eq!(items[2].as_str().unwrap(), "c");
        assert_eq!(items[3].as_str().unwrap(), "d");
    }

    #[test]
    fn test_merge_yaml_scalar_override() {
        let mut base: serde_yaml_ng::Value = serde_yaml_ng::from_str("name: base").unwrap();
        let overlay: serde_yaml_ng::Value = serde_yaml_ng::from_str("name: overlay").unwrap();
        merge_yaml(&mut base, &overlay);
        assert_eq!(base["name"].as_str().unwrap(), "overlay");
    }

    // -----------------------------------------------------------------------
    // load_config with includes tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_config_includes_field_parses() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            "project_name: myproject\nincludes:\n  - extra.yaml\ncrates: []\n",
        )
        .unwrap();
        let extra_path = tmp.path().join("extra.yaml");
        fs::write(&extra_path, "report_sizes: true\n").unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.project_name, "myproject");
        assert_eq!(
            config.includes,
            Some(vec![anodizer_core::config::IncludeSpec::Path(
                "extra.yaml".to_string()
            )])
        );
        assert_eq!(config.report_sizes, Some(true));
    }

    #[test]
    fn test_load_config_includes_merges_base_and_include() {
        let tmp = TempDir::new().unwrap();

        // Include file defines a dist override
        let include_path = tmp.path().join("overrides.yaml");
        fs::write(&include_path, "dist: /custom/dist\n").unwrap();

        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            "project_name: merged\nincludes:\n  - overrides.yaml\ncrates: []\n",
        )
        .unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.project_name, "merged");
        assert_eq!(config.dist, std::path::PathBuf::from("/custom/dist"));
    }

    #[test]
    fn test_load_config_includes_sequences_concatenated() {
        let tmp = TempDir::new().unwrap();

        let include_path = tmp.path().join("more-crates.yaml");
        fs::write(
            &include_path,
            "crates:\n  - name: extra-crate\n    path: crates/extra\n",
        )
        .unwrap();

        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            "project_name: seq-test\nincludes:\n  - more-crates.yaml\ncrates:\n  - name: base-crate\n    path: crates/base\n",
        )
        .unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.crates.len(), 2);
        // Includes are accumulated as defaults first; base is merged on top,
        // so base sequences are appended after include sequences.
        assert_eq!(config.crates[0].name, "extra-crate");
        assert_eq!(config.crates[1].name, "base-crate");
    }

    #[test]
    fn test_load_config_base_wins_over_include_for_scalar() {
        let tmp = TempDir::new().unwrap();

        // Include file defines a dist that should be treated as a default.
        let include_path = tmp.path().join("defaults.yaml");
        fs::write(&include_path, "dist: /from-include\n").unwrap();

        // Base config also defines dist — it should win.
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            "project_name: priority-test\nincludes:\n  - defaults.yaml\ndist: /from-base\ncrates: []\n",
        )
        .unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(
            config.dist,
            std::path::PathBuf::from("/from-base"),
            "base config should override include for scalar values"
        );
    }

    #[test]
    fn test_load_config_missing_include_file_returns_error() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            "project_name: test\nincludes:\n  - nonexistent.yaml\ncrates: []\n",
        )
        .unwrap();

        let result = load_config(&cfg_path);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("nonexistent.yaml") || msg.contains("include"),
            "unexpected error message: {}",
            msg
        );
    }

    #[test]
    fn test_load_config_no_includes_works_as_before() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(&cfg_path, "project_name: simple\ncrates: []\n").unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.project_name, "simple");
        assert!(config.includes.is_none());
    }

    #[test]
    fn test_load_config_includes_recursive_two_level() {
        // a.yaml includes b.yaml; b.yaml includes c.yaml. Every level
        // should contribute fields to the merged config.
        let tmp = TempDir::new().unwrap();

        let c_path = tmp.path().join("c.yaml");
        fs::write(&c_path, "dist: /from-c\nreport_sizes: true\n").unwrap();

        let b_path = tmp.path().join("b.yaml");
        fs::write(
            &b_path,
            "includes:\n  - c.yaml\ncrates:\n  - name: from-b\n    path: crates/b\n",
        )
        .unwrap();

        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            "project_name: recursive\nincludes:\n  - b.yaml\ncrates:\n  - name: base\n    path: crates/base\n",
        )
        .unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.project_name, "recursive");
        assert_eq!(
            config.dist,
            std::path::PathBuf::from("/from-c"),
            "c.yaml's scalar value should propagate up two levels"
        );
        assert_eq!(
            config.report_sizes,
            Some(true),
            "c.yaml's report_sizes should propagate up"
        );
        // Sequence concatenation order: c (no crates) → b (from-b) → base.
        let names: Vec<&str> = config.crates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["from-b", "base"],
            "crates concat in declaration order with base last"
        );
    }

    #[test]
    fn test_load_config_includes_cycle_detected() {
        // a -> b -> a should bail with a "cycle detected" error.
        let tmp = TempDir::new().unwrap();
        let b_path = tmp.path().join("b.yaml");
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(&b_path, "includes:\n  - anodizer.yaml\n").unwrap();
        fs::write(
            &cfg_path,
            "project_name: cycle\nincludes:\n  - b.yaml\ncrates: []\n",
        )
        .unwrap();

        let err = load_config(&cfg_path).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("cycle detected"),
            "expected cycle-detected error, got: {msg}"
        );
    }

    /// A config that includes itself directly (A -> A) must error with a
    /// self-cycle message. Without the dedicated check, the root key
    /// pre-inserted into `visited` would silently dedup the include into
    /// an empty mapping and the malformed config would parse cleanly.
    #[test]
    fn test_load_config_includes_self_cycle() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            "project_name: self\nincludes:\n  - anodizer.yaml\ncrates: []\n",
        )
        .unwrap();

        let err = load_config(&cfg_path).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("self-cycle"),
            "expected self-cycle error, got: {msg}"
        );
    }

    #[test]
    fn test_load_config_includes_path_relative_to_included_file() {
        // a.yaml includes nested/b.yaml; b.yaml includes c.yaml — which
        // lives in `nested/` next to b.yaml, NOT next to a.yaml.
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("nested");
        fs::create_dir_all(&nested).unwrap();

        let c_path = nested.join("c.yaml");
        fs::write(&c_path, "dist: /from-nested-c\n").unwrap();

        let b_path = nested.join("b.yaml");
        fs::write(&b_path, "includes:\n  - c.yaml\nreport_sizes: true\n").unwrap();

        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            "project_name: rel\nincludes:\n  - nested/b.yaml\ncrates: []\n",
        )
        .unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(
            config.dist,
            std::path::PathBuf::from("/from-nested-c"),
            "second-level include resolved relative to its own directory"
        );
    }

    /// `~user/...` (POSIX user-home form) is intentionally NOT expanded
    /// — only `~/` and `$VAR` are recognized. A path like `~bob/foo`
    /// must round-trip unchanged so the downstream `read_to_string`
    /// surfaces the missing-file error, rather than us guessing at
    /// `bob`'s home and silently rewriting the user's path.
    #[test]
    fn test_expand_path_tilde_user_form_not_supported() {
        let got = expand_path_tilde_and_env("~bob/foo");
        assert_eq!(
            got, "~bob/foo",
            "~user/... must NOT be expanded (POSIX user-home form unsupported)"
        );
        let got_no_slash = expand_path_tilde_and_env("~bob");
        assert_eq!(
            got_no_slash, "~bob",
            "~user with no trailing slash must NOT be expanded either"
        );
    }

    #[test]
    fn test_load_config_includes_dedup_same_file_twice() {
        // The same include listed twice should load once — sequences
        // wouldn't double, scalars wouldn't drift, and the visit set
        // suppresses the second pass.
        let tmp = TempDir::new().unwrap();
        let extra = tmp.path().join("extra.yaml");
        fs::write(&extra, "crates:\n  - name: only-once\n    path: crates/x\n").unwrap();
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            "project_name: dedup\nincludes:\n  - extra.yaml\n  - extra.yaml\ncrates: []\n",
        )
        .unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(
            config.crates.len(),
            1,
            "duplicate include should only contribute once"
        );
    }

    // ---- Version validation in load_config ----

    #[test]
    fn test_load_config_version_1_accepted() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(&cfg_path, "project_name: test\nversion: 1\ncrates: []\n").unwrap();
        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.version, Some(1));
    }

    #[test]
    fn test_load_config_version_2_accepted() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(&cfg_path, "project_name: test\nversion: 2\ncrates: []\n").unwrap();
        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.version, Some(2));
    }

    #[test]
    fn test_load_config_version_99_rejected() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(&cfg_path, "project_name: test\nversion: 99\ncrates: []\n").unwrap();
        let result = load_config(&cfg_path);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unsupported config version"),
            "error should mention unsupported version: {}",
            msg
        );
    }

    #[test]
    fn test_load_config_env_files_list_form() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            "project_name: test\nenv_files:\n  - .env\n  - .release.env\ncrates: []\n",
        )
        .unwrap();
        let config = load_config(&cfg_path).unwrap();
        let env_files = config.env_files.unwrap();
        let files = env_files
            .as_list()
            .unwrap_or_else(|| panic!("expected List variant"));
        assert_eq!(files, &[".env", ".release.env"]);
    }

    #[test]
    fn test_load_config_env_files_struct_form() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            "project_name: test\nenv_files:\n  github_token: /tmp/gh_token\n  gitlab_token: /tmp/gl_token\ncrates: []\n",
        )
        .unwrap();
        let config = load_config(&cfg_path).unwrap();
        let env_files = config.env_files.unwrap();
        let tokens = env_files
            .as_token_files()
            .unwrap_or_else(|| panic!("expected TokenFiles variant"));
        assert_eq!(tokens.github_token.as_deref(), Some("/tmp/gh_token"));
        assert_eq!(tokens.gitlab_token.as_deref(), Some("/tmp/gl_token"));
        assert!(tokens.gitea_token.is_none());
    }

    #[test]
    fn test_load_config_with_ignore_and_overrides() {
        // defaults.ignore / defaults.overrides live under
        // defaults.builds (path-mirror BuildConfig).
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
  builds:
    ignore:
      - os: windows
        arch: arm64
    overrides:
      - targets: ["x86_64-*"]
        features: [simd]
crates: []
"#,
        )
        .unwrap();
        let config = load_config(&cfg_path).unwrap();
        let builds = config.defaults.unwrap().builds.unwrap();
        assert_eq!(builds.ignore.unwrap().len(), 1);
        assert_eq!(builds.overrides.unwrap().len(), 1);
    }

    // -----------------------------------------------------------------------
    // Structured includes (from_file, from_url) tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_includes_from_file_structured_form() {
        let tmp = TempDir::new().unwrap();

        let include_path = tmp.path().join("shared.yaml");
        fs::write(&include_path, "report_sizes: true\n").unwrap();

        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            "project_name: structured\nincludes:\n  - from_file:\n      path: shared.yaml\ncrates: []\n",
        )
        .unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.project_name, "structured");
        assert_eq!(config.report_sizes, Some(true));
        // The includes field itself should deserialize as FromFile variant
        assert_eq!(
            config.includes,
            Some(vec![anodizer_core::config::IncludeSpec::FromFile {
                from_file: anodizer_core::config::IncludeFilePath {
                    path: "shared.yaml".to_string(),
                },
            }])
        );
    }

    #[test]
    fn test_includes_mixed_string_and_structured() {
        let tmp = TempDir::new().unwrap();

        let extra1 = tmp.path().join("extra1.yaml");
        fs::write(&extra1, "report_sizes: true\n").unwrap();

        let extra2 = tmp.path().join("extra2.yaml");
        fs::write(&extra2, "dist: /custom\n").unwrap();

        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            r#"project_name: mixed
includes:
  - extra1.yaml
  - from_file:
      path: extra2.yaml
crates: []
"#,
        )
        .unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.project_name, "mixed");
        assert_eq!(config.report_sizes, Some(true));
        assert_eq!(config.dist, std::path::PathBuf::from("/custom"));
        assert_eq!(config.includes.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn test_includes_from_file_absolute_path_rejected() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            format!(
                "project_name: test\nincludes:\n  - from_file:\n      path: {}\ncrates: []\n",
                if cfg!(windows) {
                    "C:\\Windows\\System32\\drivers\\etc\\hosts"
                } else {
                    "/etc/passwd"
                }
            ),
        )
        .unwrap();

        let result = load_config(&cfg_path);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("absolute paths are not allowed"),
            "expected absolute path error, got: {}",
            msg
        );
    }

    #[test]
    fn test_includes_from_file_missing_path_field() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            "project_name: test\nincludes:\n  - from_file:\n      wrong_key: value\ncrates: []\n",
        )
        .unwrap();

        let result = load_config(&cfg_path);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("invalid entry")
                || msg.contains("missing field")
                || msg.contains("from_file"),
            "expected invalid entry error, got: {}",
            msg
        );
    }

    #[test]
    fn test_includes_backward_compat_plain_strings() {
        // This is the critical backward-compatibility test: existing configs
        // with simple string includes must continue to work exactly as before.
        let tmp = TempDir::new().unwrap();

        let inc1 = tmp.path().join("inc1.yaml");
        fs::write(&inc1, "dist: /from-inc1\n").unwrap();

        let inc2 = tmp.path().join("inc2.yaml");
        fs::write(&inc2, "report_sizes: true\n").unwrap();

        let cfg_path = tmp.path().join("anodizer.yaml");
        fs::write(
            &cfg_path,
            "project_name: backcompat\nincludes:\n  - inc1.yaml\n  - inc2.yaml\ncrates: []\n",
        )
        .unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.project_name, "backcompat");
        assert_eq!(config.dist, std::path::PathBuf::from("/from-inc1"));
        assert_eq!(config.report_sizes, Some(true));
    }

    // -----------------------------------------------------------------------
    // normalize_include_url tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_normalize_url_full_https() {
        let result = normalize_include_url("https://example.com/config.yaml");
        assert_eq!(result, "https://example.com/config.yaml");
    }

    #[test]
    fn test_normalize_url_full_http() {
        let result = normalize_include_url("http://internal.corp/config.yaml");
        assert_eq!(result, "http://internal.corp/config.yaml");
    }

    #[test]
    fn test_normalize_url_github_shorthand() {
        let result = normalize_include_url("caarlos0/goreleaserfiles/main/packages.yml");
        assert_eq!(
            result,
            "https://raw.githubusercontent.com/caarlos0/goreleaserfiles/main/packages.yml"
        );
    }

    #[test]
    fn test_normalize_url_github_shorthand_complex() {
        let result = normalize_include_url("org/repo/branch/path/to/config.yaml");
        assert_eq!(
            result,
            "https://raw.githubusercontent.com/org/repo/branch/path/to/config.yaml"
        );
    }

    // -----------------------------------------------------------------------
    // TOML includes with structured form
    // -----------------------------------------------------------------------

    #[test]
    fn test_toml_includes_plain_string_backward_compat() {
        let tmp = TempDir::new().unwrap();

        let include_path = tmp.path().join("defaults.yaml");
        fs::write(&include_path, "report_sizes: true\n").unwrap();

        let cfg_path = tmp.path().join("anodizer.toml");
        fs::write(
            &cfg_path,
            "project_name = \"toml-test\"\nincludes = [\"defaults.yaml\"]\ncrates = []\n",
        )
        .unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.project_name, "toml-test");
        assert_eq!(config.report_sizes, Some(true));
    }

    #[test]
    fn test_toml_includes_from_file_structured() {
        let tmp = TempDir::new().unwrap();

        let include_path = tmp.path().join("shared.yaml");
        fs::write(&include_path, "dist: /shared-dist\n").unwrap();

        let cfg_path = tmp.path().join("anodizer.toml");
        fs::write(
            &cfg_path,
            r#"project_name = "toml-structured"
crates = []

[[includes]]
[includes.from_file]
path = "shared.yaml"
"#,
        )
        .unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.project_name, "toml-structured");
        assert_eq!(config.dist, std::path::PathBuf::from("/shared-dist"));
    }

    // -----------------------------------------------------------------------
    // Fix #5: Header keys NOT expanded, only values are
    // -----------------------------------------------------------------------

    #[test]
    fn test_header_keys_not_expanded_only_values() {
        // Drive `expand_with` against a closed lookup map so the test never
        // touches process env. The production header pipeline calls
        // `expand_env_vars` (which routes through `std::env::var`); the
        // contract this test pins is the value-vs-key expansion shape,
        // not the lookup backend.
        let lookup = |name: &str| match name {
            "ANODIZER_HDR_VAL" => Some("expanded_val".to_string()),
            _ => None,
        };

        let mut headers = std::collections::HashMap::new();
        headers.insert(
            "$KEY_LITERAL".to_string(),
            "${ANODIZER_HDR_VAL}".to_string(),
        );

        let key = "$KEY_LITERAL";
        let value = "${ANODIZER_HDR_VAL}";
        assert_eq!(
            key, "$KEY_LITERAL",
            "header key must be preserved literally"
        );
        assert_eq!(
            anodizer_core::env_expand::expand_with(value, lookup),
            "expanded_val",
            "header value must be expanded"
        );
        // Verify that expanding the key WOULD destroy it (returns empty since
        // KEY_LITERAL is not set in the lookup), proving we must NOT expand keys.
        assert_eq!(
            anodizer_core::env_expand::expand_with(key, lookup),
            "",
            "expanding a key with valid var name destroys it — proves keys must not be expanded"
        );
    }

    // -----------------------------------------------------------------------
    // Fix #8: from_url error path (unreachable URL)
    // -----------------------------------------------------------------------

    #[test]
    fn test_fetch_url_unreachable_returns_error() {
        // Use a clearly invalid URL that will fail to connect.
        let result = fetch_url_as_yaml(
            "http://127.0.0.1:1/nonexistent.yaml",
            None,
            Path::new("test-config.yaml"),
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("failed to fetch include URL") || msg.contains("127.0.0.1"),
            "expected connection error, got: {}",
            msg
        );
    }

    // -----------------------------------------------------------------------
    // Fix #10: TOML from_url structured form in TOML config
    // -----------------------------------------------------------------------

    #[test]
    fn test_toml_includes_from_url_structured_form() {
        // Verify the TOML [[includes]] / [includes.from_url] syntax parses correctly.
        // We test via file-based include since we can't easily test HTTP, but we
        // verify the TOML structure is correctly converted to YAML for resolve_include.
        let tmp = TempDir::new().unwrap();

        // Use a from_file to prove the TOML structured form works (from_url would
        // need a server; the conversion path is identical).
        let include_path = tmp.path().join("shared.yaml");
        fs::write(&include_path, "report_sizes: true\n").unwrap();

        let cfg_path = tmp.path().join("anodizer.toml");
        fs::write(
            &cfg_path,
            r#"project_name = "toml-from-url-test"
crates = []

[[includes]]
[includes.from_url]
url = "https://example.com/config.yaml"
"#,
        )
        .unwrap();

        // This will fail at fetch time (no server), but the TOML parsing and
        // IncludeSpec deserialization should work. We test that separately.
        let config_result = load_config(&cfg_path);
        // We expect an error from the URL fetch, not from parsing
        assert!(config_result.is_err());
        let msg = config_result.unwrap_err().to_string();
        assert!(
            msg.contains("fetch") || msg.contains("include URL"),
            "should fail at fetch, not parse: {}",
            msg
        );
    }

    // -----------------------------------------------------------------------
    // Regression: full-`Config` deserialization must not depend on the
    // caller's thread stack size. Debug-built `serde_yaml_ng::from_value::
    // <Config>` and `toml::from_str::<Config>` consume several MiB of stack
    // because each generated visitor branch lives in one monomorphised
    // frame. Routing through `core::config::deserialize_on_worker` keeps
    // every caller safe regardless of the host's main-thread reservation
    // (Windows: 1 MiB). The test below pins the contract by invoking
    // `load_config` from a deliberately small (256 KiB) caller thread.
    // Pre-fix this overflows on every platform under debug builds; post-fix
    // it succeeds because the worker thread carries its own 8 MiB stack.
    // -----------------------------------------------------------------------

    #[test]
    fn load_config_succeeds_on_small_caller_stack() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join(".anodizer.yaml");
        fs::write(
            &cfg_path,
            r#"version: 2
project_name: stack-probe
crates:
  - name: demo
    path: .
    tag_template: "v{{ Version }}"
"#,
        )
        .unwrap();

        // 512 KiB is half the Windows main-thread reservation and well
        // below the ~768 KiB pre-fix threshold where debug builds of the
        // monolithic `Config` visitor overflow on Linux. Post-fix the
        // deserialize is routed through the helper's 8 MiB worker so the
        // outer 512 KiB budget only has to cover the YAML-Value parse,
        // the include-merge walk, and the per-CrateConfig JSON round-trips
        // inside `defaults_merge`, each comfortably small.
        let cfg_path_string = cfg_path.to_string_lossy().to_string();
        let handle = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .name("load_config_small_stack_probe".to_string())
            .spawn(move || {
                load_config(std::path::Path::new(&cfg_path_string))
                    .map(|c| c.project_name)
                    .map_err(|e| e.to_string())
            })
            .expect("spawn small-stack probe thread");
        let result = handle.join().expect("probe thread did not panic");
        assert_eq!(
            result.as_deref(),
            Ok("stack-probe"),
            "load_config must succeed from a small caller stack: {:?}",
            result
        );
    }

    #[test]
    fn find_config_in_finds_anodizer_yaml_under_base() {
        // The primary path: a base dir carrying `.anodizer.yaml` resolves to
        // exactly that joined path (not a cwd-relative one), so the cwd-free
        // probe matches `find_config`'s candidate ordering without mutating
        // the process cwd.
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join(".anodizer.yaml");
        fs::write(&cfg, "project_name: based\ncrates: []\n").unwrap();

        let found = find_config_in(tmp.path()).expect("must find the config under base");
        assert_eq!(found, cfg);
    }

    #[test]
    fn find_config_in_falls_back_to_cargo_toml() {
        // No anodizer config but a Cargo.toml present: return the joined
        // Cargo.toml path so `load_config` recognizes the fallback by
        // filename and yields a default Config.
        let tmp = TempDir::new().unwrap();
        let cargo = tmp.path().join("Cargo.toml");
        fs::write(&cargo, "[package]\nname = \"x\"\nversion = \"0.1.0\"\n").unwrap();

        let found = find_config_in(tmp.path()).expect("must fall back to Cargo.toml");
        assert_eq!(found, cargo);
        // Round-trip through load_repo_config: the Cargo.toml fallback yields
        // a default Config (empty project_name), matching load_config's
        // special-case.
        let cfg = load_repo_config(tmp.path()).expect("load_repo_config must succeed");
        assert!(cfg.project_name.is_empty());
    }

    #[test]
    fn find_config_in_bails_when_neither_present() {
        // Empty base dir: no anodizer config, no Cargo.toml → hard error
        // naming the searched directory.
        let tmp = TempDir::new().unwrap();
        let err = find_config_in(tmp.path())
            .expect_err("empty dir must bail")
            .to_string();
        assert!(
            err.contains("no anodizer config file found"),
            "error must explain the miss: {err}"
        );
        // load_repo_config propagates the same miss.
        assert!(load_repo_config(tmp.path()).is_err());
    }

    #[test]
    fn load_repo_config_loads_yaml_under_base() {
        // Full find + load round-trip from a base dir: the parsed config's
        // project_name proves the right file was located and deserialized
        // without any cwd change.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".anodizer.yaml"),
            "project_name: loaded-from-base\ncrates: []\n",
        )
        .unwrap();

        let cfg = load_repo_config(tmp.path()).expect("must load the config under base");
        assert_eq!(cfg.project_name, "loaded-from-base");
    }
}
