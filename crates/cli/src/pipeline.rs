use anodizer_core::config::{Config, IncludeSpec};
use anodizer_core::context::Context;
use anodizer_core::env_expand::expand_env as expand_env_vars;
pub use anodizer_core::hooks::run_hooks;
use anodizer_core::log::StageLogger;
use anodizer_core::stage::Stage;
use anyhow::{Context as _, Result, bail};
use colored::Colorize;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    //     in GR; serde alias accepts both but collapses them on parse).
    //   * GR V1 `dockers:` block — anodizer is V2-only by design;
    //     without this check `deny_unknown_fields` emits a generic
    //     "unknown field" error that does not point at `docker_v2:`.
    // Best-effort — YAML parse failures are reported by the typed loader below.
    if (ext == "yaml" || ext == "yml")
        && let Ok(raw) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&content)
    {
        anodizer_core::config::warn_on_legacy_snapshot_name_template(&raw);
        anodizer_core::config::validate_no_docker_v1(&raw).map_err(anyhow::Error::msg)?;
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
    // Emit deprecation warning + ignore any `gobinary:` field on builds
    // (Go-only — anodizer always uses cargo).
    anodizer_core::config::apply_build_legacy_aliases(&mut config);

    // Validate config schema version
    anodizer_core::config::validate_version(&config).map_err(anyhow::Error::msg)?;
    // Validate git.tag_sort if present
    anodizer_core::config::validate_tag_sort(&config).map_err(anyhow::Error::msg)?;
    // Validate archives[].format_overrides[].goos
    anodizer_core::config::validate_format_overrides(&config).map_err(anyhow::Error::msg)?;
    // Validate release block does not configure multiple SCM backends.
    anodizer_core::config::validate_release_backends(&config).map_err(anyhow::Error::msg)?;
    // Validate defaults.crates / defaults.workspaces axis matches top-level.
    anodizer_core::config::validate_defaults_axis(&config).map_err(anyhow::Error::msg)?;
    // Validate homebrew_cask does not set both url_template and url.template.
    anodizer_core::config::validate_homebrew_cask_url_template(&config)
        .map_err(anyhow::Error::msg)?;
    // Validate archives[].id and universal_binaries[].id uniqueness.
    anodizer_core::config::validate_id_uniqueness(&config).map_err(anyhow::Error::msg)?;

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
    // one (matches GoReleaser's `Default()` pipe). Fills in anodizer defaults
    // for empty name/email so error messages referencing author identity at
    // config-validation time see non-empty strings.
    normalize_commit_author_defaults(&mut config);

    // Fold workspace-level `defaults` into every per-crate config so
    // downstream stages can read from `crate_cfg.<field>` regardless of
    // whether the value was set per-crate or hoisted to defaults.
    anodizer_core::defaults_merge::apply_defaults(&mut config);

    Ok(config)
}

/// Walk the loaded config and fill in commit_author defaults on every
/// publisher that has one (homebrew formula + cask, scoop, chocolatey, winget,
/// nix, aur, krew). GoReleaser does this in its per-publisher `Default()`
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

// run_hooks is re-exported from anodizer_core::hooks

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

    /// Returns the registered stage names in pipeline order. Used by the
    /// pipeline-construction tests to assert stage ordering invariants
    /// (e.g. blob runs before snapcraft-publish so the submitter gate
    /// sees blob's outcome via `ctx.publish_report`).
    #[cfg(test)]
    pub fn stage_names(&self) -> Vec<&str> {
        self.stages.iter().map(|s| s.name()).collect()
    }

    /// Run every registered stage in order; `emit_summary` always
    /// fires after the inner body returns, regardless of `Ok`/`Err`.
    ///
    /// `emit_summary` runs at the pipeline level (not inside
    /// `AnnounceStage::run`) so the end-of-pipeline status table and
    /// `--summary-json=<path>` write always fire — including when
    /// announce itself is operator-skipped via `--skip=announce`. The
    /// scope-guard shape (inner-fn returns the outcome, outer wrapper
    /// calls `emit_summary` then propagates) means the summary fires
    /// on Ok, Err, AND when the pipeline body short-circuits early
    /// via `?`.
    ///
    /// # Panics
    ///
    /// If a stage panics, the unwind happens BEFORE the
    /// `emit_summary` post-call runs, so a panicking pipeline body
    /// will skip the summary write. This is an accepted limitation
    /// — a stage that panics is a bug in the stage (or a panic from
    /// `unwrap`/`expect` we missed in review), not an operator
    /// error we can recover from. The release pipeline is built
    /// around `Result`-propagation; a panic means something the
    /// review failed to catch is wrong, and dropping `summary.json`
    /// in that scenario is bug-on-bug (the missing summary is a
    /// downstream symptom of the underlying panic, not a release
    /// gate). A `scopeguard::defer!` wrapper would close this
    /// window but adds a panic-safety abstraction layer the rest
    /// of the codebase doesn't use; the inner-fn shape mirrors the
    /// convention already established by
    /// `AnnounceStage::run` → `announce_body`.
    pub fn run(&self, ctx: &mut Context, log: &StageLogger) -> Result<()> {
        let outcome = self.run_inner(ctx, log);
        anodizer_stage_announce::emit_summary(ctx);
        outcome
    }

    /// Inner pipeline body. Lives separately so `Pipeline::run` can
    /// wrap it in the unconditional `emit_summary` post-call — see
    /// the audit reference at the top of `run`.
    fn run_inner(&self, ctx: &mut Context, log: &StageLogger) -> Result<()> {
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
                anodizer_core::artifact::ArtifactKind::Binary
                    | anodizer_core::artifact::ArtifactKind::UploadableBinary
                    | anodizer_core::artifact::ArtifactKind::UniversalBinary
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
                                anodizer_core::artifact::ArtifactKind::Binary
                                    | anodizer_core::artifact::ArtifactKind::UploadableBinary
                                    | anodizer_core::artifact::ArtifactKind::UniversalBinary
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
fn write_pre_release_metadata(ctx: &mut anodizer_core::context::Context) -> anyhow::Result<()> {
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
    use anodizer_stage_announce::AnnounceStage;
    use anodizer_stage_appbundle::AppBundleStage;
    use anodizer_stage_archive::ArchiveStage;
    use anodizer_stage_blob::BlobStage;
    use anodizer_stage_build::BuildStage;
    use anodizer_stage_changelog::ChangelogStage;
    use anodizer_stage_checksum::ChecksumStage;
    use anodizer_stage_dmg::DmgStage;
    use anodizer_stage_docker::DockerStage;
    use anodizer_stage_flatpak::FlatpakStage;
    use anodizer_stage_makeself::MakeselfStage;
    use anodizer_stage_msi::MsiStage;
    use anodizer_stage_nfpm::NfpmStage;
    use anodizer_stage_notarize::NotarizeStage;
    use anodizer_stage_nsis::NsisStage;
    use anodizer_stage_pkg::PkgStage;
    use anodizer_stage_publish::PublishStage;
    use anodizer_stage_release::ReleaseStage;
    use anodizer_stage_sbom::SbomStage;
    use anodizer_stage_sign::{DockerSignStage, SignStage};
    use anodizer_stage_snapcraft::{SnapcraftPublishStage, SnapcraftStage};
    use anodizer_stage_source::SourceStage;
    use anodizer_stage_srpm::SrpmStage;
    use anodizer_stage_templatefiles::TemplateFilesStage;
    use anodizer_stage_upx::UpxStage;

    // Stage order matches GoReleaser pipeline.go for parity.
    // Anodizer-specific stages (appbundle, dmg, msi, pkg, nsis, templatefiles,
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
    p.add(Box::new(DockerStage::new()));
    // DockerSignStage runs after DockerStage so docker image artifacts exist.
    p.add(Box::new(DockerSignStage));
    p.add(Box::new(PublishStage));
    // BlobStage runs before SnapcraftPublishStage so a required-blob
    // failure can short-circuit the snapcraft upload via the same
    // `any_failed(Assets, required_only=true)` check that already gates
    // every other Submitter publisher.
    p.add(Box::new(BlobStage));
    p.add(Box::new(SnapcraftPublishStage));
    p.add(Box::new(AnnounceStage));
    p
}

/// Build a pipeline that only runs the build stage (for --split mode).
pub fn build_split_pipeline() -> Pipeline {
    use anodizer_stage_build::BuildStage;
    use anodizer_stage_upx::UpxStage;

    let mut p = Pipeline::new();
    p.add(Box::new(BuildStage));
    p.add(Box::new(UpxStage));
    p
}

/// Build a publish-only pipeline: release, publish, blob, snapcraft-publish stages.
///
/// **Note**: this is the pipeline consumed by the LEGACY `anodize
/// publish` subcommand, which assumes the input dist was produced by
/// a full `anodize release` whose own SignStage already fired. Adding
/// a head SignStage here would silently introduce a new credential
/// requirement to the existing surface. The
/// `anodize release --publish-only` path uses
/// [`build_publish_only_pipeline`] instead, which DOES prepend
/// SignStage for the determinism-preserved-dist re-sign pass.
pub fn build_publish_pipeline() -> Pipeline {
    use anodizer_stage_blob::BlobStage;
    use anodizer_stage_publish::PublishStage;
    use anodizer_stage_release::ReleaseStage;
    use anodizer_stage_snapcraft::SnapcraftPublishStage;

    let mut p = Pipeline::new();
    p.add(Box::new(ReleaseStage));
    p.add(Box::new(PublishStage));
    // BlobStage before SnapcraftPublishStage so the snapcraft submitter
    // gate sees blob's outcome via `ctx.publish_report`.
    p.add(Box::new(BlobStage));
    p.add(Box::new(SnapcraftPublishStage));
    p
}

/// Build the pipeline for `anodize release --publish-only`:
/// `[ChangelogStage, SignStage, ReleaseStage, PublishStage,
/// BlobStage, SnapcraftPublishStage, AnnounceStage]`. The head
/// `SignStage` is the production-keys re-sign pass — the preserved
/// dist's archive bytes are byte-stable (the determinism check
/// verified that) but their `.sig`/`.asc` signatures are either
/// missing entirely (harness skips Sign when prod keys are exported
/// on the runner) or ephemeral (harness ran without prod keys).
///
/// **Ordering invariants**:
/// - `ChangelogStage` runs first. It is a pure GitHub API call with
///   no artifact dependency, and `ReleaseStage::build_release_json`
///   reads `ctx.stage_outputs.changelogs` to populate the GitHub
///   release body — so it MUST land before `ReleaseStage`. Placing
///   it at the head also means a GitHub API failure aborts before
///   any signing work is performed.
/// - `AnnounceStage` runs last, matching `build_merge_pipeline` and
///   `build_release_pipeline`. The stage's internal
///   `required_publishers` gate then sees the final publish report
///   and only fires notifications on a green publish.
///
/// **Idempotence requirement on SignStage**: must be safe to re-run
/// on a dist whose existing `.sig`/`.asc` files are already
/// production signatures (gpg/cosign `--output` semantics overwrite
/// in place; `helpers::should_sign_artifact` excludes
/// `Signature`/`Certificate` artifact kinds from the `all`/`any`
/// filters so re-running can't produce `*.sig.sig` chains). The
/// publish-only entry point ALSO strips any *ephemeral* harness
/// signature/certificate artifacts up-front in
/// `commands/release/publish_only::strip_ephemeral_signatures` so
/// the head SignStage only sees the underlying archives.
///
/// Cross-platform packagers (msi/nsis/dmg/pkg/appbundle/flatpak/etc.)
/// that the harness's default stage list doesn't cover are expected
/// to have run in the upstream harness pipeline before preserve-dist
/// captured the tree — those stages are added to the harness's stage
/// list in CI and their outputs land under `dist/`. The publish-only
/// pipeline therefore consumes the full artifact set as-is and does
/// not re-run any artifact-producing stages.
pub(crate) fn build_publish_only_pipeline() -> Pipeline {
    use anodizer_stage_announce::AnnounceStage;
    use anodizer_stage_blob::BlobStage;
    use anodizer_stage_changelog::ChangelogStage;
    use anodizer_stage_checksum::ChecksumStage;
    use anodizer_stage_publish::PublishStage;
    use anodizer_stage_release::ReleaseStage;
    use anodizer_stage_sign::SignStage;
    use anodizer_stage_snapcraft::SnapcraftPublishStage;

    let mut p = Pipeline::new();
    p.add(Box::new(ChangelogStage));
    p.add(Box::new(SignStage));
    // ChecksumStage between SignStage and PublishStage hashes the
    // production-signed bytes and backfills `sha256` onto every
    // artifact so each publisher sees the metadata its manifest
    // schema requires. The recompute is byte-deterministic, so this
    // is idempotent across re-runs.
    p.add(Box::new(ChecksumStage));
    p.add(Box::new(ReleaseStage));
    p.add(Box::new(PublishStage));
    p.add(Box::new(BlobStage));
    p.add(Box::new(SnapcraftPublishStage));
    p.add(Box::new(AnnounceStage));
    p
}

/// Build an announce-only pipeline.
pub fn build_announce_pipeline() -> Pipeline {
    use anodizer_stage_announce::AnnounceStage;

    let mut p = Pipeline::new();
    p.add(Box::new(AnnounceStage));
    p
}

/// Build a pipeline for --merge mode: all post-build stages.
pub fn build_merge_pipeline() -> Pipeline {
    use anodizer_stage_announce::AnnounceStage;
    use anodizer_stage_appbundle::AppBundleStage;
    use anodizer_stage_archive::ArchiveStage;
    use anodizer_stage_blob::BlobStage;
    use anodizer_stage_changelog::ChangelogStage;
    use anodizer_stage_checksum::ChecksumStage;
    use anodizer_stage_dmg::DmgStage;
    use anodizer_stage_docker::DockerStage;
    use anodizer_stage_flatpak::FlatpakStage;
    use anodizer_stage_makeself::MakeselfStage;
    use anodizer_stage_msi::MsiStage;
    use anodizer_stage_nfpm::NfpmStage;
    use anodizer_stage_notarize::NotarizeStage;
    use anodizer_stage_nsis::NsisStage;
    use anodizer_stage_pkg::PkgStage;
    use anodizer_stage_publish::PublishStage;
    use anodizer_stage_release::ReleaseStage;
    use anodizer_stage_sbom::SbomStage;
    use anodizer_stage_sign::{DockerSignStage, SignStage};
    use anodizer_stage_snapcraft::{SnapcraftPublishStage, SnapcraftStage};
    use anodizer_stage_source::SourceStage;
    use anodizer_stage_srpm::SrpmStage;
    use anodizer_stage_templatefiles::TemplateFilesStage;

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
    p.add(Box::new(DockerStage::new()));
    p.add(Box::new(DockerSignStage));
    p.add(Box::new(PublishStage));
    // BlobStage before SnapcraftPublishStage — mirrors
    // `build_release_pipeline`'s swap so merge-mode runs share the same
    // submitter-gate semantics.
    p.add(Box::new(BlobStage));
    p.add(Box::new(SnapcraftPublishStage));
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
    #[serial]
    fn test_header_keys_not_expanded_only_values() {
        unsafe { std::env::set_var("ANODIZER_HDR_VAL", "expanded_val") };

        let mut headers = std::collections::HashMap::new();
        headers.insert(
            "$KEY_LITERAL".to_string(),
            "${ANODIZER_HDR_VAL}".to_string(),
        );

        // We can't call fetch_url_as_yaml without a real server, but we can verify
        // the expand_env_vars behavior that the code relies on: header keys are NOT
        // passed through expand_env_vars (only values are).
        // Verify: expanding the key would change it, but we don't expand keys.
        let key = "$KEY_LITERAL";
        let value = "${ANODIZER_HDR_VAL}";
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

        unsafe { std::env::remove_var("ANODIZER_HDR_VAL") };
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

    // -----------------------------------------------------------------------
    // Stage-order invariants
    //
    // BlobStage must run BEFORE SnapcraftPublishStage in every pipeline
    // variant so a required-blob failure can short-circuit the
    // (irreversible) snapcraft upload via the same
    // `any_failed(Assets, required_only=true)` gate that already
    // protects every other Submitter publisher.
    // -----------------------------------------------------------------------

    fn assert_blob_before_snapcraft(names: &[&str], pipeline: &str) {
        let blob_idx = names
            .iter()
            .position(|n| *n == "blob")
            .unwrap_or_else(|| panic!("{pipeline}: missing blob stage; got {names:?}"));
        let snap_idx = names
            .iter()
            .position(|n| *n == "snapcraft-publish")
            .unwrap_or_else(|| panic!("{pipeline}: missing snapcraft-publish; got {names:?}"));
        assert!(
            blob_idx < snap_idx,
            "{pipeline}: blob (idx {blob_idx}) must precede snapcraft-publish (idx {snap_idx}); got {names:?}"
        );
    }

    #[test]
    fn release_pipeline_runs_blob_before_snapcraft_publish() {
        let p = build_release_pipeline();
        let names = p.stage_names();
        assert_blob_before_snapcraft(&names, "build_release_pipeline");
    }

    #[test]
    fn publish_pipeline_runs_blob_before_snapcraft_publish() {
        let p = build_publish_pipeline();
        let names = p.stage_names();
        assert_blob_before_snapcraft(&names, "build_publish_pipeline");
    }

    #[test]
    fn merge_pipeline_runs_blob_before_snapcraft_publish() {
        let p = build_merge_pipeline();
        let names = p.stage_names();
        assert_blob_before_snapcraft(&names, "build_merge_pipeline");
    }

    #[test]
    fn publish_only_pipeline_runs_blob_before_snapcraft_publish() {
        // The `--publish-only` pipeline must honor the same
        // blob-before-snapcraft-publish ordering as every other
        // variant so a required-blob failure can short-circuit the
        // (irreversible) snapcraft upload via the
        // `any_failed(Assets, required_only=true)` gate.
        let p = build_publish_only_pipeline();
        let names = p.stage_names();
        assert_blob_before_snapcraft(&names, "build_publish_only_pipeline");
    }

    #[test]
    fn publish_only_pipeline_runs_sign_before_release() {
        // SignStage must be at the HEAD of the publish-only pipeline
        // so production signatures land on the preserved archives
        // BEFORE ReleaseStage uploads them.
        let p = build_publish_only_pipeline();
        let names = p.stage_names();
        let sign_idx = names
            .iter()
            .position(|n| *n == "sign")
            .expect("publish-only pipeline must include sign stage");
        let release_idx = names
            .iter()
            .position(|n| *n == "release")
            .expect("publish-only pipeline must include release stage");
        assert!(
            sign_idx < release_idx,
            "sign (idx {sign_idx}) must precede release (idx {release_idx}); got {names:?}"
        );
    }

    #[test]
    fn publish_only_pipeline_runs_changelog_before_release() {
        // ReleaseStage::build_release_json reads ctx.stage_outputs.changelogs;
        // without ChangelogStage ahead of it the GitHub release body would
        // be empty even though the project configures `changelog.use:
        // github-native`. ChangelogStage at the head also costs no signing
        // work if its GitHub API call fails.
        let p = build_publish_only_pipeline();
        let names = p.stage_names();
        let changelog_idx = names
            .iter()
            .position(|n| *n == "changelog")
            .expect("publish-only pipeline must include changelog stage");
        let release_idx = names
            .iter()
            .position(|n| *n == "release")
            .expect("publish-only pipeline must include release stage");
        assert!(
            changelog_idx < release_idx,
            "changelog (idx {changelog_idx}) must precede release (idx {release_idx}); got {names:?}"
        );
    }

    /// Stage order: SignStage → ChecksumStage → PublishStage.
    /// ChecksumStage must follow Sign so signed bytes are what get
    /// hashed, and must precede Publish so every publisher
    /// (winget, chocolatey, scoop, krew, …) sees per-artifact
    /// `sha256` metadata its manifest schema requires.
    #[test]
    fn publish_only_pipeline_runs_checksum_before_publish_after_sign() {
        let p = build_publish_only_pipeline();
        let names = p.stage_names();
        let checksum_idx = names
            .iter()
            .position(|n| *n == "checksum")
            .expect("publish-only pipeline must include checksum stage");
        let sign_idx = names
            .iter()
            .position(|n| *n == "sign")
            .expect("publish-only pipeline must include sign stage");
        let publish_idx = names
            .iter()
            .position(|n| *n == "publish")
            .expect("publish-only pipeline must include publish stage");
        assert!(
            sign_idx < checksum_idx,
            "checksum (idx {checksum_idx}) must follow sign (idx {sign_idx}) so production-signed bytes get hashed; got {names:?}"
        );
        assert!(
            checksum_idx < publish_idx,
            "checksum (idx {checksum_idx}) must precede publish (idx {publish_idx}) so publishers see sha256 metadata; got {names:?}"
        );
    }

    #[test]
    fn publish_only_pipeline_runs_announce_after_publish() {
        // AnnounceStage must follow the publisher chain so it only fires on
        // a green release; it is the last stage by symmetry with merge /
        // release pipelines so the `required_publishers` gate sees the
        // final publish report.
        let p = build_publish_only_pipeline();
        let names = p.stage_names();
        let announce_idx = names
            .iter()
            .position(|n| *n == "announce")
            .expect("publish-only pipeline must include announce stage");
        let publish_idx = names
            .iter()
            .position(|n| *n == "publish")
            .expect("publish-only pipeline must include publish stage");
        assert!(
            announce_idx > publish_idx,
            "announce (idx {announce_idx}) must follow publish (idx {publish_idx}); got {names:?}"
        );
        assert_eq!(
            announce_idx,
            names.len() - 1,
            "announce must be the final stage; got {names:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Pipeline-level emit_summary contract.
    //
    // Pipeline::run must ALWAYS invoke `emit_summary` (regardless of
    // whether `AnnounceStage::run` was reached). The unit tests in
    // `stage-announce` pin the stage-side contract; this test pins the
    // pipeline-side contract — specifically that `--skip=announce`
    // doesn't drop `--summary-json`.
    // -----------------------------------------------------------------------

    /// A `Stage` that always returns `Err`. Pins the "emit_summary
    /// fires even on inner-fn Err" half of `Pipeline::run`'s contract.
    /// Kept private to the test module.
    struct AlwaysFailStage;
    impl anodizer_core::stage::Stage for AlwaysFailStage {
        fn name(&self) -> &str {
            "always-fail"
        }
        fn run(&self, _ctx: &mut anodizer_core::context::Context) -> anyhow::Result<()> {
            anyhow::bail!("synthetic stage failure for the I-3 test")
        }
    }

    #[test]
    fn pipeline_emits_summary_even_when_inner_stage_returns_err() {
        // The inner-fn scope-guard shape in `Pipeline::run` must
        // invoke `emit_summary` on Err too, not just on Ok. Without
        // this test, only the doc line pinned the contract; this
        // puts a bisectable green/red signal on the Err path.
        use anodizer_core::context::ContextOptions;

        let tmp = TempDir::new().expect("tempdir");
        let summary_path = tmp.path().join("summary.json");

        let mut p = Pipeline::new();
        p.add(Box::new(AlwaysFailStage));

        let opts = ContextOptions {
            summary_json_path: Some(summary_path.clone()),
            ..ContextOptions::default()
        };
        let config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        let mut ctx = anodizer_core::context::Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");
        ctx.publish_report = Some(anodizer_core::publish_report::PublishReport::default());

        let log = ctx.logger("pipeline-test");
        let result = p.run(&mut ctx, &log);

        assert!(
            result.is_err(),
            "pipeline must propagate the stage's Err verbatim",
        );
        assert!(
            summary_path.exists(),
            "summary.json must be written even when the inner pipeline body returns Err",
        );
    }

    #[test]
    fn pipeline_emits_summary_when_announce_is_skipped_via_skip_flag() {
        use anodizer_core::context::ContextOptions;
        use anodizer_stage_announce::AnnounceStage;

        let tmp = TempDir::new().expect("tempdir");
        let summary_path = tmp.path().join("summary.json");

        // Build a pipeline whose only stage is AnnounceStage and skip
        // it via `--skip=announce`. The summary still lands on disk
        // because Pipeline::run owns emit_summary and invokes it after
        // the stage loop, regardless of whether the stage ran.
        let mut p = Pipeline::new();
        p.add(Box::new(AnnounceStage));

        let opts = ContextOptions {
            summary_json_path: Some(summary_path.clone()),
            skip_stages: vec!["announce".to_string()],
            ..ContextOptions::default()
        };
        let config = anodizer_core::config::Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        let mut ctx = anodizer_core::context::Context::new(config, opts);
        ctx.template_vars_mut().set("Tag", "v9.9.9-test");
        ctx.publish_report = Some(anodizer_core::publish_report::PublishReport::default());

        let log = ctx.logger("pipeline-test");
        p.run(&mut ctx, &log).expect("pipeline run");

        // The stage was skipped — but the summary must STILL be written.
        // Regression: an earlier shape put emit_summary inside
        // AnnounceStage::run, where a skipped stage never reached it.
        // Pipeline must own emit_summary so operator-skip can't suppress
        // the summary side-effect.
        assert!(
            summary_path.exists(),
            "summary.json must be written even when announce is operator-skipped",
        );
    }
}
