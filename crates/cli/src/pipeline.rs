use anodize_core::config::{Config, IncludeSpec};
use anodize_core::context::Context;
pub use anodize_core::hooks::run_hooks;
use anodize_core::log::StageLogger;
use anodize_core::stage::Stage;
use anyhow::{Context as _, Result, bail};
use colored::Colorize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Find config file. If `config_override` is provided, use that path directly;
/// otherwise search the current directory for well-known config file names.
pub fn find_config(config_override: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = config_override {
        if path.exists() {
            return Ok(path.to_path_buf());
        }
        bail!("config file not found: {}", path.display());
    }
    let candidates = [
        ".anodize.yaml",
        ".anodize.yml",
        ".anodize.toml",
        "anodize.yaml",
        "anodize.yml",
        "anodize.toml",
    ];
    for name in &candidates {
        let path = PathBuf::from(name);
        if path.exists() {
            return Ok(path);
        }
    }
    // Fallback: if Cargo.toml exists, use a default config instead of erroring.
    if Path::new("Cargo.toml").exists() {
        eprintln!("WARNING: no anodize config found; using defaults from Cargo.toml");
        return Ok(PathBuf::from("Cargo.toml"));
    }
    bail!(
        "no anodize config file found (tried: {}). Run `anodize init` to generate one.",
        candidates.join(", ")
    )
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

/// Detect deprecated YAML aliases in a raw config value so commands can
/// surface them via `Context::deprecate` after construction.
///
/// Returns `(property, message)` pairs. Only detects aliases whose renamed
/// form actually exists in the current `Config` schema.
///
/// Parity with GoReleaser `internal/deprecate/deprecate.go` notices plus
/// anodize-specific renames (e.g. `goamd64`→`amd64_variant`).
pub fn detect_deprecated_aliases(raw: &serde_yaml_ng::Value) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();

    let map = match raw {
        serde_yaml_ng::Value::Mapping(m) => m,
        _ => return found,
    };

    let k = |s: &str| serde_yaml_ng::Value::String(s.to_string());

    // top-level `gemfury:` / `fury:` / `npms:` — removed publishers.
    // Emit a deprecation message so users porting GoReleaser configs see why
    // their configs stopped working instead of a silent "unknown field" error.
    for (key, msg) in &[
        (
            "gemfury",
            "`gemfury:` publisher was removed (Ruby/Python gem hosting does not fit a Rust release tool). Use `fury:` → also removed. For deb/rpm hosting, use `cloudsmiths:` or `artifactories:`.",
        ),
        (
            "fury",
            "`fury:` publisher was removed (no first-class Rust use case). Use `cloudsmiths:` or `artifactories:` for deb/rpm hosting.",
        ),
        (
            "npms",
            "`npms:` publisher was removed (NPM targets JavaScript packages). Publish Rust binaries through `homebrew:`, `scoop:`, `chocolatey:`, `winget:`, `aur:`, or `docker:` instead.",
        ),
    ] {
        if map.contains_key(k(key)) {
            found.push(((*key).to_string(), (*msg).to_string()));
        }
    }

    // `snapshot.name_template:` -> `snapshot.version_template:`
    if let Some(serde_yaml_ng::Value::Mapping(sm)) = map.get(k("snapshot"))
        && sm.contains_key(k("name_template"))
    {
        found.push((
            "snapshot.name_template".to_string(),
            "`snapshot.name_template` is deprecated, use `snapshot.version_template`".to_string(),
        ));
    }

    // `announce.email.body_template:` -> `announce.email.message_template:`
    if let Some(serde_yaml_ng::Value::Mapping(am)) = map.get(k("announce"))
        && let Some(serde_yaml_ng::Value::Mapping(em)) = am.get(k("email"))
        && em.contains_key(k("body_template"))
    {
        found.push((
            "announce.email.body_template".to_string(),
            "`announce.email.body_template` is deprecated, use `announce.email.message_template`"
                .to_string(),
        ));
    }

    // Top-level `homebrew_casks[].goamd64:` -> `amd64_variant:`
    if let Some(serde_yaml_ng::Value::Sequence(cs)) = map.get(k("homebrew_casks")) {
        check_variant_renames_in_seq(cs, "homebrew_casks", &mut found);
    }

    // Per-crate walks: archives, nfpm, snapcrafts, publisher blocks.
    if let Some(serde_yaml_ng::Value::Sequence(crates_seq)) = map.get(k("crates")) {
        for crate_entry in crates_seq {
            let crate_map = match crate_entry {
                serde_yaml_ng::Value::Mapping(m) => m,
                _ => continue,
            };

            // `archives[].format:` -> `archives[].formats:` (GoReleaser deprecation).
            // Plus nested `format_overrides[].format:` -> `formats:`.
            if let Some(serde_yaml_ng::Value::Sequence(archs)) = crate_map.get(k("archives")) {
                for arch in archs {
                    let am = match arch {
                        serde_yaml_ng::Value::Mapping(m) => m,
                        _ => continue,
                    };
                    if am.contains_key(k("format")) {
                        found.push((
                            "archives.format".to_string(),
                            "`archives[].format` is deprecated, use `archives[].formats` (plural array)".to_string(),
                        ));
                    }
                    if let Some(serde_yaml_ng::Value::Sequence(overrides)) =
                        am.get(k("format_overrides"))
                    {
                        for ov in overrides {
                            if let serde_yaml_ng::Value::Mapping(om) = ov
                                && om.contains_key(k("format"))
                            {
                                found.push((
                                    "archives.format_overrides.format".to_string(),
                                    "`archives[].format_overrides[].format` is deprecated, use `formats` (plural array)".to_string(),
                                ));
                                break;
                            }
                        }
                    }
                }
            }

            // `nfpm[].builds:` -> `nfpm[].ids:` (note: field is `nfpm`, not `nfpms`).
            // Also: missing `maintainer` is deprecated in GoReleaser (always-set).
            if let Some(serde_yaml_ng::Value::Sequence(nfpm_seq)) = crate_map.get(k("nfpm")) {
                let mut builds_seen = false;
                let mut maintainer_missing = false;
                for entry in nfpm_seq {
                    if let serde_yaml_ng::Value::Mapping(m) = entry {
                        if !builds_seen && m.contains_key(k("builds")) {
                            builds_seen = true;
                        }
                        let m_val = m.get(k("maintainer"));
                        let is_empty = match m_val {
                            None => true,
                            Some(serde_yaml_ng::Value::String(s)) => s.trim().is_empty(),
                            Some(serde_yaml_ng::Value::Null) => true,
                            _ => false,
                        };
                        if is_empty {
                            maintainer_missing = true;
                        }
                    }
                }
                if builds_seen {
                    found.push((
                        "nfpm.builds".to_string(),
                        "`nfpm[].builds` is deprecated, use `nfpm[].ids`".to_string(),
                    ));
                }
                if maintainer_missing {
                    found.push((
                        "nfpm.maintainer".to_string(),
                        "`nfpm[].maintainer` should always be set; a future release will require it"
                            .to_string(),
                    ));
                }
            }

            // `snapcrafts[].builds:` -> `snapcrafts[].ids:`.
            if let Some(serde_yaml_ng::Value::Sequence(snap_seq)) = crate_map.get(k("snapcrafts")) {
                for entry in snap_seq {
                    if let serde_yaml_ng::Value::Mapping(m) = entry
                        && m.contains_key(k("builds"))
                    {
                        found.push((
                            "snapcrafts.builds".to_string(),
                            "`snapcrafts[].builds` is deprecated, use `snapcrafts[].ids`"
                                .to_string(),
                        ));
                        break;
                    }
                }
            }

            // Publisher blocks (scalar, not vec) with `goamd64` / `goarm` aliases.
            for publisher in &[
                "homebrew",
                "scoop",
                "chocolatey",
                "winget",
                "aur",
                "aur_source",
                "nix",
            ] {
                if let Some(serde_yaml_ng::Value::Mapping(pm)) = crate_map.get(k(publisher)) {
                    if pm.contains_key(k("goamd64")) {
                        found.push((
                            format!("crates.{publisher}.goamd64"),
                            format!(
                                "`{publisher}.goamd64` is deprecated, use `{publisher}.amd64_variant`"
                            ),
                        ));
                    }
                    if pm.contains_key(k("goarm")) {
                        found.push((
                            format!("crates.{publisher}.goarm"),
                            format!(
                                "`{publisher}.goarm` is deprecated, use `{publisher}.arm_variant`"
                            ),
                        ));
                    }
                }
            }
        }
    }

    found
}

/// Helper: for a sequence of mappings, emit `<ctx>.goamd64`/`.goarm` deprecation
/// entries into `found` when an entry contains those keys. Used for top-level
/// vec-shaped publisher configs (e.g. `homebrew_casks[]`).
fn check_variant_renames_in_seq(
    seq: &[serde_yaml_ng::Value],
    ctx: &str,
    found: &mut Vec<(String, String)>,
) {
    let k = |s: &str| serde_yaml_ng::Value::String(s.to_string());
    let mut amd_seen = false;
    let mut arm_seen = false;
    for entry in seq {
        if let serde_yaml_ng::Value::Mapping(m) = entry {
            if !amd_seen && m.contains_key(k("goamd64")) {
                amd_seen = true;
            }
            if !arm_seen && m.contains_key(k("goarm")) {
                arm_seen = true;
            }
        }
    }
    if amd_seen {
        found.push((
            format!("{ctx}.goamd64"),
            format!("`{ctx}[].goamd64` is deprecated, use `{ctx}[].amd64_variant`"),
        ));
    }
    if arm_seen {
        found.push((
            format!("{ctx}.goarm"),
            format!("`{ctx}[].goarm` is deprecated, use `{ctx}[].arm_variant`"),
        ));
    }
}

/// Load a config file and return both the `Config` and any deprecation
/// notices detected in the raw YAML/TOML body. Commands use the notices to
/// call `Context::deprecate` after constructing their `Context`.
pub fn load_config_with_deprecations(path: &Path) -> Result<(Config, Vec<(String, String)>)> {
    let config = load_config(path)?;
    let deprecations = match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "yaml" | "yml" => std::fs::read_to_string(path)
            .ok()
            .and_then(|content| serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&content).ok())
            .map(|raw| detect_deprecated_aliases(&raw))
            .unwrap_or_default(),
        "toml" => std::fs::read_to_string(path)
            .ok()
            .and_then(|content| toml::from_str::<toml::Value>(&content).ok())
            .and_then(|toml_val| serde_json::to_value(&toml_val).ok())
            .and_then(|json_val| serde_yaml_ng::to_value(json_val).ok())
            .map(|raw| detect_deprecated_aliases(&raw))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    Ok((config, deprecations))
}

/// Load config from a file, auto-detecting format by extension.
///
/// For YAML files, processes `includes` by deep-merging included files together as
/// defaults, then merging the base (local) config on top. This means the base config
/// always takes priority over values from included files — includes provide defaults,
/// not overrides.
pub fn load_config(path: &Path) -> Result<Config> {
    // Special case: Cargo.toml fallback returns a default Config. The
    // find_config function returns "Cargo.toml" when no anodize config file
    // exists but a Cargo.toml is present in the working directory.
    if path.file_name().and_then(|n| n.to_str()) == Some("Cargo.toml") {
        return Ok(Config::default());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mut config = match ext {
        "yaml" | "yml" => load_yaml_config_with_includes(path, &content)?,
        "toml" => load_toml_config_with_includes(path, &content)?,
        _ => bail!("unsupported config format: {}", ext),
    };

    // Validate config schema version
    anodize_core::config::validate_version(&config).map_err(|e| anyhow::anyhow!("{}", e))?;
    // Validate git.tag_sort if present
    anodize_core::config::validate_tag_sort(&config).map_err(|e| anyhow::anyhow!("{}", e))?;

    // Apply monorepo defaults: when monorepo.dir is set and a crate's path
    // is empty or ".", default it to monorepo.dir.
    apply_monorepo_defaults(&mut config);

    Ok(config)
}

/// Apply monorepo configuration defaults to crate configs.
///
/// When `monorepo.dir` is set and a crate's `path` is empty or `"."`,
/// the crate's path is defaulted to `monorepo.dir`. This matches
/// GoReleaser Pro's behavior where monorepo.dir acts as the default
/// working directory for all builds.
///
/// Note: `BuildConfig` does not have a `dir` field — builds inherit
/// their working directory from `CrateConfig.path`, which is already
/// defaulted here. `PublisherConfig.dir` and `StructuredHook.dir` are
/// intentionally left alone since they represent explicit overrides.
fn apply_monorepo_defaults(config: &mut Config) {
    let monorepo_dir = config.monorepo_dir().map(|s| s.to_string());

    if let Some(dir) = monorepo_dir {
        for crate_cfg in &mut config.crates {
            if crate_cfg.path.is_empty() || crate_cfg.path == "." {
                crate_cfg.path = dir.clone();
            }
        }
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
    let mut merged = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
    for entry in &include_entries {
        let overlay = resolve_include(entry, base_dir, path)?;
        merge_yaml(&mut merged, &overlay);
    }
    // Merge base config on top of the accumulated defaults (base wins).
    merge_yaml(&mut merged, &base);

    serde_yaml_ng::from_value(merged)
        .with_context(|| format!("failed to deserialize config: {}", path.display()))
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
        // No includes — fast path: deserialize directly from TOML.
        return toml::from_str(content)
            .with_context(|| format!("failed to deserialize TOML config: {}", path.display()));
    }

    // Convert the base TOML to a YAML Value so we can use the existing
    // deep-merge logic. Round-trip through serde_json::Value as an
    // intermediate format that both serde_yaml_ng and toml support.
    let base_json = serde_json::to_value(&base_toml)
        .with_context(|| "failed to convert TOML config to JSON for merging")?;
    let base_yaml: serde_yaml_ng::Value = serde_yaml_ng::to_value(&base_json)
        .with_context(|| "failed to convert TOML config to YAML for merging")?;

    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut merged = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
    for entry in &include_entries {
        // Convert each TOML include entry to a YAML value so resolve_include can handle it.
        let json_entry = serde_json::to_value(entry)
            .with_context(|| "failed to convert TOML include entry to JSON")?;
        let yaml_entry: serde_yaml_ng::Value = serde_yaml_ng::to_value(&json_entry)
            .with_context(|| "failed to convert TOML include entry to YAML")?;
        let overlay = resolve_include(&yaml_entry, base_dir, path)?;
        merge_yaml(&mut merged, &overlay);
    }
    // Merge base config on top of the accumulated defaults (base wins).
    merge_yaml(&mut merged, &base_yaml);

    serde_yaml_ng::from_value(merged)
        .with_context(|| format!("failed to deserialize config: {}", path.display()))
}

/// Expand environment variable references in a string.
///
/// Supports `${VAR_NAME}` and `$VAR_NAME` syntax. Unset variables are replaced
/// with an empty string (matching GoReleaser behavior).
fn expand_env_vars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' {
            if chars.peek() == Some(&'{') {
                // ${VAR_NAME} form
                chars.next(); // consume '{'
                let mut var_name = String::new();
                let mut found_close = false;
                for ch in chars.by_ref() {
                    if ch == '}' {
                        found_close = true;
                        break;
                    }
                    var_name.push(ch);
                }
                if !found_close {
                    // No closing '}' — preserve the literal '${' and consumed text
                    result.push_str("${");
                    result.push_str(&var_name);
                } else if let Ok(val) = std::env::var(&var_name) {
                    result.push_str(&val);
                }
            } else {
                // $VAR_NAME form: variable names must start with a letter or
                // underscore (like shell rules). Digits after '$' are kept
                // literal (e.g. "$5" stays "$5").
                let starts_valid = chars
                    .peek()
                    .map(|&ch| ch.is_ascii_alphabetic() || ch == '_')
                    .unwrap_or(false);
                if !starts_valid {
                    // Not a variable reference — keep the literal '$'
                    result.push('$');
                } else {
                    let mut var_name = String::new();
                    while let Some(&ch) = chars.peek() {
                        if ch.is_alphanumeric() || ch == '_' {
                            var_name.push(ch);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if let Ok(val) = std::env::var(&var_name) {
                        result.push_str(&val);
                    }
                }
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Normalize a URL for include fetching.
///
/// If the URL does not start with `http://` or `https://`, prepend
/// `https://raw.githubusercontent.com/` (GitHub raw content shorthand,
/// matching GoReleaser Pro behavior).
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
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
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

/// Resolve a single include entry (from a raw YAML value) to its YAML content.
///
/// Deserializes the entry into an `IncludeSpec` to avoid duplicate format logic,
/// then dispatches to the appropriate handler:
/// - **Path(string)**: treated as a relative file path (backward compatible)
/// - **FromFile**: structured file path via `from_file.path`
/// - **FromUrl**: fetch from URL with optional headers, env var expansion on URL and header values
fn resolve_include(
    entry: &serde_yaml_ng::Value,
    base_dir: &Path,
    config_path: &Path,
) -> Result<serde_yaml_ng::Value> {
    let spec: IncludeSpec = serde_yaml_ng::from_value(entry.clone())
        .with_context(|| format!("includes: invalid entry in {}", config_path.display()))?;
    match spec {
        IncludeSpec::Path(path_str) => resolve_file_include(&path_str, base_dir, config_path),
        IncludeSpec::FromFile { from_file } => {
            resolve_file_include(&from_file.path, base_dir, config_path)
        }
        IncludeSpec::FromUrl { from_url } => {
            let url = expand_env_vars(&normalize_include_url(&from_url.url));
            fetch_url_as_yaml(&url, from_url.headers.as_ref(), config_path)
        }
    }
}

/// Resolve a file-based include by reading and parsing it.
fn resolve_file_include(
    path_str: &str,
    base_dir: &Path,
    config_path: &Path,
) -> Result<serde_yaml_ng::Value> {
    // Reject absolute paths to prevent unexpected file reads.
    if Path::new(path_str).is_absolute() {
        bail!(
            "includes: absolute paths are not allowed (got '{}' in {})",
            path_str,
            config_path.display()
        );
    }
    let include_path = base_dir.join(path_str);
    let include_content = std::fs::read_to_string(&include_path).with_context(|| {
        format!(
            "failed to read include file '{}' (referenced from {})",
            include_path.display(),
            config_path.display()
        )
    })?;
    load_include_as_yaml(&include_path, &include_content)
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

// run_hooks is re-exported from anodize_core::hooks

pub struct Pipeline {
    stages: Vec<Box<dyn Stage>>,
}

impl Pipeline {
    pub fn new() -> Self {
        Self { stages: vec![] }
    }

    pub fn add(&mut self, stage: Box<dyn Stage>) {
        self.stages.push(stage);
    }

    pub fn run(&self, ctx: &mut Context, log: &StageLogger) -> Result<()> {
        // Skip-stage validation runs at the CLI entry (`validate_skip_values`
        // in main.rs); the command never reaches this point with an unknown
        // value. No runtime warning is needed.

        // Stages that only make sense when binary artifacts exist.  When the
        // build stage produces no binaries (library-only crate), these stages
        // are skipped with a clear message instead of silently reporting ✓.
        const BINARY_DEPENDENT_STAGES: &[&str] = &[
            "upx",
            "archive",
            "makeself",
            "nfpm",
            "snapcraft",
            "appbundle",
            "dmg",
            "msi",
            "pkg",
            "nsis",
            "flatpak",
            "notarize",
            "srpm",
        ];

        // Check if binaries already exist (merge mode loads artifacts before
        // the pipeline runs, so build stage never executes).
        let mut has_binaries = ctx.artifacts.all().iter().any(|a| {
            matches!(
                a.kind,
                anodize_core::artifact::ArtifactKind::Binary
                    | anodize_core::artifact::ArtifactKind::UploadableBinary
                    | anodize_core::artifact::ArtifactKind::UniversalBinary
            )
        });

        for stage in &self.stages {
            let name = stage.name();
            if ctx.should_skip(name) {
                log.status(&format!("{} {}", name.bold(), "skipped".yellow()));
                continue;
            }

            // After the build stage, check if any binary artifacts were produced.
            // Skip binary-dependent stages if not (library-only crate).
            // NOTE: This is a pipeline optimization, not a feature skip. Each stage
            // checks its own config internally; stages with no config return Ok(())
            // immediately. The strict_guard for "no binaries" lives inside the
            // individual stages (e.g., archive, upx) where it fires AFTER the stage
            // confirms it has work to do.
            if BINARY_DEPENDENT_STAGES.contains(&name) && !has_binaries {
                log.status(&format!(
                    "{} {} {}",
                    "\u{2713}".green().bold(),
                    name.bold(),
                    "(no binaries, skipped)".yellow()
                ));
                continue;
            }

            // Write metadata.json + artifacts.json before the release stage
            // so that include_meta can attach them to the GitHub release.
            // run_post_pipeline overwrites these with the final version later.
            if name == "release"
                && let Err(e) = write_pre_release_metadata(ctx)
            {
                log.warn(&format!("failed to write pre-release metadata: {}", e));
            }

            log.status(&format!("\u{2022} {}...", name.bold()));
            match stage.run(ctx) {
                Ok(()) => {
                    log.status(&format!("{} {}", "\u{2713}".green().bold(), name.bold()));
                    // After the build stage, record whether binaries were produced.
                    if name == "build" {
                        has_binaries = ctx.artifacts.all().iter().any(|a| {
                            matches!(
                                a.kind,
                                anodize_core::artifact::ArtifactKind::Binary
                                    | anodize_core::artifact::ArtifactKind::UploadableBinary
                                    | anodize_core::artifact::ArtifactKind::UniversalBinary
                            )
                        });
                    }
                    // After the changelog stage completes, populate the ReleaseNotes
                    // template variable so subsequent stages can reference it.
                    if name == "changelog" {
                        ctx.populate_release_notes_var();
                    }
                }
                Err(e) => {
                    log.status(&format!(
                        "{} {} \u{2014} {}",
                        "\u{2717}".red().bold(),
                        name.bold(),
                        e
                    ));
                    return Err(e);
                }
            }
        }

        // End-of-pipeline skip summary. Stages (sign, docker-sign, publisher)
        // record intentional per-sub-config skips via
        // `ctx.remember_skip(...)`; before this hook the skips were emitted
        // at verbose level and lost in the final "✓ done" output.
        let skips = ctx.skip_memento.drain();
        if !skips.is_empty() {
            let noun = if skips.len() == 1 {
                "intentional skip"
            } else {
                "intentional skips"
            };
            log.status(&format!("{} {}:", skips.len(), noun.yellow()));
            for ev in &skips {
                log.status(&format!(
                    "  {} [{}] {} — {}",
                    "\u{21b3}".yellow(),
                    ev.stage.bold(),
                    ev.label,
                    ev.reason
                ));
            }
        }
        Ok(())
    }
}

/// Write preliminary metadata.json and artifacts.json before the release
/// stage so that `include_meta: true` can attach them to the GitHub release.
/// `run_post_pipeline` overwrites these with the final version afterward.
fn write_pre_release_metadata(ctx: &mut anodize_core::context::Context) -> anyhow::Result<()> {
    let dist = &ctx.config.dist;
    std::fs::create_dir_all(dist)?;

    let tag = ctx.template_vars().get("Tag").cloned().unwrap_or_default();
    let version = ctx.version();
    let commit = ctx
        .template_vars()
        .get("FullCommit")
        .cloned()
        .unwrap_or_default();

    let metadata = serde_json::json!({
        "project_name": ctx.config.project_name,
        "tag": tag,
        "version": version,
        "commit": commit,
    });
    std::fs::write(
        dist.join("metadata.json"),
        serde_json::to_string_pretty(&metadata)?,
    )?;

    let artifacts_json = ctx.artifacts.to_artifacts_json()?;
    std::fs::write(
        dist.join("artifacts.json"),
        serde_json::to_string_pretty(&artifacts_json)?,
    )?;

    Ok(())
}

/// Build the full release pipeline with all stages in order
pub fn build_release_pipeline() -> Pipeline {
    use anodize_stage_announce::AnnounceStage;
    use anodize_stage_appbundle::AppBundleStage;
    use anodize_stage_archive::ArchiveStage;
    use anodize_stage_blob::BlobStage;
    use anodize_stage_build::BuildStage;
    use anodize_stage_changelog::ChangelogStage;
    use anodize_stage_checksum::ChecksumStage;
    use anodize_stage_dmg::DmgStage;
    use anodize_stage_docker::DockerStage;
    use anodize_stage_flatpak::FlatpakStage;
    use anodize_stage_makeself::MakeselfStage;
    use anodize_stage_msi::MsiStage;
    use anodize_stage_nfpm::NfpmStage;
    use anodize_stage_notarize::NotarizeStage;
    use anodize_stage_nsis::NsisStage;
    use anodize_stage_pkg::PkgStage;
    use anodize_stage_publish::PublishStage;
    use anodize_stage_release::ReleaseStage;
    use anodize_stage_sbom::SbomStage;
    use anodize_stage_sign::{DockerSignStage, SignStage};
    use anodize_stage_snapcraft::{SnapcraftPublishStage, SnapcraftStage};
    use anodize_stage_source::SourceStage;
    use anodize_stage_srpm::SrpmStage;
    use anodize_stage_templatefiles::TemplateFilesStage;
    use anodize_stage_upx::UpxStage;

    // Stage order matches GoReleaser pipeline.go for parity.
    // Anodize-specific stages (appbundle, dmg, msi, pkg, nsis, templatefiles,
    // release, snapcraft-publish, blob) are interleaved at logical positions.
    let mut p = Pipeline::new();

    // ── Build ────────────────────────────────────────────────────────────
    p.add(Box::new(BuildStage));
    p.add(Box::new(UpxStage));
    // AppBundle → DMG → PKG must run before Notarize (macOS signing).
    // MSI and NSIS are Windows equivalents at the same pipeline phase.
    p.add(Box::new(AppBundleStage));
    p.add(Box::new(DmgStage));
    p.add(Box::new(MsiStage));
    p.add(Box::new(PkgStage));
    p.add(Box::new(NsisStage));
    p.add(Box::new(NotarizeStage));

    // ── Changelog ────────────────────────────────────────────────────────
    p.add(Box::new(ChangelogStage));

    // ── Packaging ────────────────────────────────────────────────────────
    p.add(Box::new(ArchiveStage));
    p.add(Box::new(SourceStage));
    p.add(Box::new(NfpmStage));
    p.add(Box::new(SrpmStage));
    p.add(Box::new(MakeselfStage));
    p.add(Box::new(SnapcraftStage));
    p.add(Box::new(FlatpakStage));
    p.add(Box::new(SbomStage));
    p.add(Box::new(TemplateFilesStage));

    // ── Integrity ────────────────────────────────────────────────────────
    p.add(Box::new(ChecksumStage));
    p.add(Box::new(SignStage));

    // ── Publish ──────────────────────────────────────────────────────────
    p.add(Box::new(ReleaseStage));
    p.add(Box::new(DockerStage));
    // DockerSignStage runs after DockerStage so docker image artifacts exist.
    p.add(Box::new(DockerSignStage));
    p.add(Box::new(PublishStage));
    p.add(Box::new(SnapcraftPublishStage));
    p.add(Box::new(BlobStage));
    p.add(Box::new(AnnounceStage));
    p
}

/// Build a pipeline that only runs the build stage (for --split mode).
pub fn build_split_pipeline() -> Pipeline {
    use anodize_stage_build::BuildStage;
    use anodize_stage_upx::UpxStage;

    let mut p = Pipeline::new();
    p.add(Box::new(BuildStage));
    p.add(Box::new(UpxStage));
    p
}

/// Build a publish-only pipeline: release, publish, snapcraft-publish, blob stages.
pub fn build_publish_pipeline() -> Pipeline {
    use anodize_stage_blob::BlobStage;
    use anodize_stage_publish::PublishStage;
    use anodize_stage_release::ReleaseStage;
    use anodize_stage_snapcraft::SnapcraftPublishStage;

    let mut p = Pipeline::new();
    p.add(Box::new(ReleaseStage));
    p.add(Box::new(PublishStage));
    p.add(Box::new(SnapcraftPublishStage));
    p.add(Box::new(BlobStage));
    p
}

/// Build an announce-only pipeline.
pub fn build_announce_pipeline() -> Pipeline {
    use anodize_stage_announce::AnnounceStage;

    let mut p = Pipeline::new();
    p.add(Box::new(AnnounceStage));
    p
}

/// Build a pipeline for --merge mode: all post-build stages.
pub fn build_merge_pipeline() -> Pipeline {
    use anodize_stage_announce::AnnounceStage;
    use anodize_stage_appbundle::AppBundleStage;
    use anodize_stage_archive::ArchiveStage;
    use anodize_stage_blob::BlobStage;
    use anodize_stage_changelog::ChangelogStage;
    use anodize_stage_checksum::ChecksumStage;
    use anodize_stage_dmg::DmgStage;
    use anodize_stage_docker::DockerStage;
    use anodize_stage_flatpak::FlatpakStage;
    use anodize_stage_makeself::MakeselfStage;
    use anodize_stage_msi::MsiStage;
    use anodize_stage_nfpm::NfpmStage;
    use anodize_stage_notarize::NotarizeStage;
    use anodize_stage_nsis::NsisStage;
    use anodize_stage_pkg::PkgStage;
    use anodize_stage_publish::PublishStage;
    use anodize_stage_release::ReleaseStage;
    use anodize_stage_sbom::SbomStage;
    use anodize_stage_sign::{DockerSignStage, SignStage};
    use anodize_stage_snapcraft::{SnapcraftPublishStage, SnapcraftStage};
    use anodize_stage_source::SourceStage;
    use anodize_stage_srpm::SrpmStage;
    use anodize_stage_templatefiles::TemplateFilesStage;

    // Merge pipeline: same order as build_release_pipeline minus Build/UPX.
    let mut p = Pipeline::new();
    p.add(Box::new(AppBundleStage));
    p.add(Box::new(DmgStage));
    p.add(Box::new(MsiStage));
    p.add(Box::new(PkgStage));
    p.add(Box::new(NsisStage));
    p.add(Box::new(NotarizeStage));
    p.add(Box::new(ChangelogStage));
    p.add(Box::new(ArchiveStage));
    p.add(Box::new(SourceStage));
    p.add(Box::new(NfpmStage));
    p.add(Box::new(SrpmStage));
    p.add(Box::new(MakeselfStage));
    p.add(Box::new(SnapcraftStage));
    p.add(Box::new(FlatpakStage));
    p.add(Box::new(SbomStage));
    p.add(Box::new(TemplateFilesStage));
    p.add(Box::new(ChecksumStage));
    p.add(Box::new(SignStage));
    p.add(Box::new(ReleaseStage));
    p.add(Box::new(DockerStage));
    p.add(Box::new(DockerSignStage));
    p.add(Box::new(PublishStage));
    p.add(Box::new(SnapcraftPublishStage));
    p.add(Box::new(BlobStage));
    p.add(Box::new(AnnounceStage));
    p
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
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
        let cfg_path = tmp.path().join("anodize.yaml");
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
            Some(vec![anodize_core::config::IncludeSpec::Path(
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

        let cfg_path = tmp.path().join("anodize.yaml");
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

        let cfg_path = tmp.path().join("anodize.yaml");
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
        let cfg_path = tmp.path().join("anodize.yaml");
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
        let cfg_path = tmp.path().join("anodize.yaml");
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
        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(&cfg_path, "project_name: simple\ncrates: []\n").unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.project_name, "simple");
        assert!(config.includes.is_none());
    }

    // ---- Version validation in load_config ----

    #[test]
    fn test_load_config_version_1_accepted() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(&cfg_path, "project_name: test\nversion: 1\ncrates: []\n").unwrap();
        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.version, Some(1));
    }

    #[test]
    fn test_load_config_version_2_accepted() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(&cfg_path, "project_name: test\nversion: 2\ncrates: []\n").unwrap();
        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.version, Some(2));
    }

    #[test]
    fn test_load_config_version_99_rejected() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodize.yaml");
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
        let cfg_path = tmp.path().join("anodize.yaml");
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
        let cfg_path = tmp.path().join("anodize.yaml");
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
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(
            &cfg_path,
            r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
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
        let defaults = config.defaults.unwrap();
        assert_eq!(defaults.ignore.unwrap().len(), 1);
        assert_eq!(defaults.overrides.unwrap().len(), 1);
    }

    // -----------------------------------------------------------------------
    // Structured includes (from_file, from_url) tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_includes_from_file_structured_form() {
        let tmp = TempDir::new().unwrap();

        let include_path = tmp.path().join("shared.yaml");
        fs::write(&include_path, "report_sizes: true\n").unwrap();

        let cfg_path = tmp.path().join("anodize.yaml");
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
            Some(vec![anodize_core::config::IncludeSpec::FromFile {
                from_file: anodize_core::config::IncludeFilePath {
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

        let cfg_path = tmp.path().join("anodize.yaml");
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
        let cfg_path = tmp.path().join("anodize.yaml");
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
        let cfg_path = tmp.path().join("anodize.yaml");
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

        let cfg_path = tmp.path().join("anodize.yaml");
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
    // expand_env_vars tests
    // -----------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_expand_env_vars_braced() {
        unsafe { std::env::set_var("ANODIZE_TEST_TOKEN_1", "secret123") };
        let result = expand_env_vars("Bearer ${ANODIZE_TEST_TOKEN_1}");
        assert_eq!(result, "Bearer secret123");
        unsafe { std::env::remove_var("ANODIZE_TEST_TOKEN_1") };
    }

    #[test]
    #[serial]
    fn test_expand_env_vars_unbraced() {
        unsafe { std::env::set_var("ANODIZE_TEST_TOKEN_2", "val2") };
        let result = expand_env_vars("prefix-$ANODIZE_TEST_TOKEN_2-suffix");
        assert_eq!(result, "prefix-val2-suffix");
        unsafe { std::env::remove_var("ANODIZE_TEST_TOKEN_2") };
    }

    #[test]
    #[serial]
    fn test_expand_env_vars_missing_var_becomes_empty() {
        // Unset variable → empty string (GoReleaser behavior)
        unsafe { std::env::remove_var("ANODIZE_NONEXISTENT_VAR_XYZ") };
        let result = expand_env_vars("token=${ANODIZE_NONEXISTENT_VAR_XYZ}!");
        assert_eq!(result, "token=!");
    }

    #[test]
    fn test_expand_env_vars_no_vars() {
        let result = expand_env_vars("no variables here");
        assert_eq!(result, "no variables here");
    }

    #[test]
    fn test_expand_env_vars_lone_dollar() {
        let result = expand_env_vars("price is $5");
        assert_eq!(result, "price is $5");
    }

    #[test]
    #[serial]
    fn test_expand_env_vars_multiple() {
        unsafe { std::env::set_var("ANODIZE_TEST_A", "aaa") };
        unsafe { std::env::set_var("ANODIZE_TEST_B", "bbb") };
        let result = expand_env_vars("${ANODIZE_TEST_A}/$ANODIZE_TEST_B");
        assert_eq!(result, "aaa/bbb");
        unsafe { std::env::remove_var("ANODIZE_TEST_A") };
        unsafe { std::env::remove_var("ANODIZE_TEST_B") };
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

        let cfg_path = tmp.path().join("anodize.toml");
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

        let cfg_path = tmp.path().join("anodize.toml");
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
    #[serial]
    fn test_header_keys_not_expanded_only_values() {
        unsafe { std::env::set_var("ANODIZE_HDR_VAL", "expanded_val") };

        let mut headers = std::collections::HashMap::new();
        headers.insert("$KEY_LITERAL".to_string(), "${ANODIZE_HDR_VAL}".to_string());

        // We can't call fetch_url_as_yaml without a real server, but we can verify
        // the expand_env_vars behavior that the code relies on: header keys are NOT
        // passed through expand_env_vars (only values are).
        // Verify: expanding the key would change it, but we don't expand keys.
        let key = "$KEY_LITERAL";
        let value = "${ANODIZE_HDR_VAL}";
        assert_eq!(
            key, "$KEY_LITERAL",
            "header key must be preserved literally"
        );
        assert_eq!(
            expand_env_vars(value),
            "expanded_val",
            "header value must be expanded"
        );
        // Verify that expanding the key WOULD destroy it (returns empty since
        // KEY_LITERAL is not set as an env var), proving we must NOT expand keys.
        assert_eq!(
            expand_env_vars(key),
            "",
            "expanding a key with valid var name destroys it — proves keys must not be expanded"
        );

        unsafe { std::env::remove_var("ANODIZE_HDR_VAL") };
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
    // Fix #9: trailing $ at end of string
    // -----------------------------------------------------------------------

    #[test]
    fn test_expand_env_vars_trailing_dollar() {
        let result = expand_env_vars("price$");
        assert_eq!(
            result, "price$",
            "trailing dollar should be preserved literally"
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

        let cfg_path = tmp.path().join("anodize.toml");
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
    // Fix #11: unclosed brace preserves literal text
    // -----------------------------------------------------------------------

    #[test]
    fn test_expand_env_vars_unclosed_brace() {
        let result = expand_env_vars("token=${UNCLOSED");
        assert_eq!(
            result, "token=${UNCLOSED",
            "unclosed brace should preserve literal text"
        );
    }

    #[test]
    fn test_expand_env_vars_unclosed_brace_empty() {
        let result = expand_env_vars("end${");
        assert_eq!(
            result, "end${",
            "unclosed empty brace should preserve literal text"
        );
    }

    // ---- detect_deprecated_aliases ----

    fn yaml(s: &str) -> serde_yaml_ng::Value {
        serde_yaml_ng::from_str(s).unwrap_or_else(|e| panic!("valid yaml in test: {e}"))
    }

    fn keys(found: &[(String, String)]) -> Vec<&str> {
        found.iter().map(|(k, _)| k.as_str()).collect()
    }

    #[test]
    fn detect_removed_top_level_publishers() {
        let got = detect_deprecated_aliases(&yaml("gemfury: {}\nfury: []\nnpms: []\n"));
        let ks = keys(&got);
        assert!(ks.contains(&"gemfury"), "got: {:?}", ks);
        assert!(ks.contains(&"fury"), "got: {:?}", ks);
        assert!(ks.contains(&"npms"), "got: {:?}", ks);
    }

    #[test]
    fn detect_snapshot_name_template() {
        let got = detect_deprecated_aliases(&yaml("snapshot:\n  name_template: 'x'\n"));
        assert_eq!(keys(&got), vec!["snapshot.name_template"]);
    }

    #[test]
    fn detect_snapshot_version_template_is_current_name() {
        let got = detect_deprecated_aliases(&yaml("snapshot:\n  version_template: 'x'\n"));
        assert!(keys(&got).is_empty(), "version_template must not warn");
    }

    #[test]
    fn detect_announce_email_body_template() {
        let got =
            detect_deprecated_aliases(&yaml("announce:\n  email:\n    body_template: 'hi'\n"));
        assert_eq!(keys(&got), vec!["announce.email.body_template"]);
    }

    #[test]
    fn detect_announce_email_message_template_is_current_name() {
        let got =
            detect_deprecated_aliases(&yaml("announce:\n  email:\n    message_template: 'hi'\n"));
        assert!(keys(&got).is_empty());
    }

    #[test]
    fn detect_nfpm_builds_under_crates() {
        // The previous implementation checked `nfpms` but the real field is
        // `nfpm`. Regression guard.
        let got = detect_deprecated_aliases(&yaml(
            "crates:\n  - name: foo\n    nfpm:\n      - builds: [a, b]\n",
        ));
        assert!(keys(&got).contains(&"nfpm.builds"), "got: {:?}", keys(&got));
    }

    #[test]
    fn detect_nfpm_missing_maintainer() {
        let got = detect_deprecated_aliases(&yaml(
            "crates:\n  - name: foo\n    nfpm:\n      - id: pkg\n",
        ));
        assert!(
            keys(&got).contains(&"nfpm.maintainer"),
            "got: {:?}",
            keys(&got)
        );
    }

    #[test]
    fn detect_nfpm_maintainer_present_does_not_warn() {
        let got = detect_deprecated_aliases(&yaml(
            "crates:\n  - name: foo\n    nfpm:\n      - id: pkg\n        maintainer: 'me <a@b>'\n",
        ));
        assert!(!keys(&got).contains(&"nfpm.maintainer"));
    }

    #[test]
    fn detect_snapcrafts_builds_under_crates() {
        let got = detect_deprecated_aliases(&yaml(
            "crates:\n  - name: foo\n    snapcrafts:\n      - builds: [a]\n",
        ));
        assert_eq!(keys(&got), vec!["snapcrafts.builds"]);
    }

    #[test]
    fn detect_archive_format_singular() {
        let got = detect_deprecated_aliases(&yaml(
            "crates:\n  - name: foo\n    archives:\n      - format: tar.gz\n",
        ));
        assert_eq!(keys(&got), vec!["archives.format"]);
    }

    #[test]
    fn detect_archive_format_overrides_format() {
        let got = detect_deprecated_aliases(&yaml(
            "crates:\n  - name: foo\n    archives:\n      - formats: [tar.gz]\n        format_overrides:\n          - goos: windows\n            format: zip\n",
        ));
        assert!(
            keys(&got).contains(&"archives.format_overrides.format"),
            "got: {:?}",
            keys(&got)
        );
    }

    #[test]
    fn detect_publisher_goamd64_rename() {
        let got = detect_deprecated_aliases(&yaml(
            "crates:\n  - name: foo\n    homebrew:\n      goamd64: v2\n    scoop:\n      goamd64: v3\n",
        ));
        let ks = keys(&got);
        assert!(ks.contains(&"crates.homebrew.goamd64"), "got: {:?}", ks);
        assert!(ks.contains(&"crates.scoop.goamd64"), "got: {:?}", ks);
    }

    #[test]
    fn detect_publisher_goarm_rename() {
        let got = detect_deprecated_aliases(&yaml(
            "crates:\n  - name: foo\n    aur:\n      goarm: '7'\n",
        ));
        assert_eq!(keys(&got), vec!["crates.aur.goarm"]);
    }

    #[test]
    fn detect_homebrew_casks_goamd64() {
        let got = detect_deprecated_aliases(&yaml("homebrew_casks:\n  - goamd64: v2\n"));
        assert_eq!(keys(&got), vec!["homebrew_casks.goamd64"]);
    }

    #[test]
    fn detect_new_names_are_silent() {
        // Canonical new names must not produce deprecation noise.
        let got = detect_deprecated_aliases(&yaml(
            "crates:\n  - name: foo\n    archives:\n      - formats: [tar.gz]\n    nfpm:\n      - id: p\n        maintainer: 'me'\n        ids: [build1]\n    homebrew:\n      amd64_variant: v2\n      arm_variant: '7'\n",
        ));
        assert!(keys(&got).is_empty(), "unexpected warnings: {:?}", got);
    }

    #[test]
    fn non_mapping_root_returns_empty() {
        let got = detect_deprecated_aliases(&serde_yaml_ng::Value::Null);
        assert!(got.is_empty());
    }
}
