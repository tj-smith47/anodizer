use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::Config;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

/// Write `dist/config.yaml` with the fully-resolved (effective) config.
///
/// This is always written, including in dry-run mode.
/// Shared by `release` and `build` pipelines so both surface the same artifact.
///
/// Two runs of the determinism harness must emit a byte-identical
/// `config.yaml`. The `Config` type carries many `HashMap<String, _>` fields
/// (`docker.labels`, `docker.build_args`, `variables`, `nfpm.dependencies`,
/// announcer `extra`, custom headers, …) whose iteration order is randomized
/// per process. We serialize to a `serde_yaml_ng::Value` first, then
/// recursively sort every mapping's keys alphabetically, then emit the
/// canonical form. Centralised here so adding a new HashMap field anywhere
/// in `Config` is automatically covered without a per-field `serialize_with`
/// attribute.
pub fn write_effective_config(config: &Config, log: &StageLogger) -> Result<()> {
    let dist = &config.dist;
    std::fs::create_dir_all(dist)
        .with_context(|| format!("failed to create dist directory: {}", dist.display()))?;
    let effective_path = dist.join("config.yaml");
    let mut value: serde_yaml_ng::Value =
        serde_yaml_ng::to_value(config).context("failed to serialize effective config")?;
    sort_yaml_mapping(&mut value);
    let yaml = serde_yaml_ng::to_string(&value).context("failed to serialize effective config")?;
    std::fs::write(&effective_path, &yaml)
        .with_context(|| format!("failed to write {}", effective_path.display()))?;
    log.verbose(&format!(
        "wrote effective config to {}",
        effective_path.display()
    ));
    Ok(())
}

/// Recursively sort every `Value::Mapping` entry by key.
///
/// `serde_yaml_ng::Mapping` is an `IndexMap` (insertion-ordered), so the
/// emit order is whatever order serde visited the source. For
/// `HashMap<String, _>` fields that order is randomized per process — fatal
/// for the determinism harness, which fingerprints `dist/config.yaml`. This
/// helper rebuilds each mapping in sort order (lexicographically by the
/// `Display` form of `Value`, which for `String` keys is the underlying
/// string — the only mapping-key shape the `Config` type produces).
pub(super) fn sort_yaml_mapping(value: &mut serde_yaml_ng::Value) {
    use serde_yaml_ng::{Mapping, Value};
    match value {
        Value::Mapping(map) => {
            let mut entries: Vec<(Value, Value)> = std::mem::take(map).into_iter().collect();
            entries.sort_by_key(|(a, _)| yaml_key_sort_key(a));
            let mut sorted = Mapping::with_capacity(entries.len());
            for (k, mut v) in entries {
                sort_yaml_mapping(&mut v);
                sorted.insert(k, v);
            }
            *map = sorted;
        }
        Value::Sequence(seq) => {
            for v in seq.iter_mut() {
                sort_yaml_mapping(v);
            }
        }
        Value::Tagged(tagged) => sort_yaml_mapping(&mut tagged.value),
        _ => {}
    }
}

/// Stable string-keyed sort for YAML mapping entries. Strings compare on
/// their UTF-8 bytes (the common case); every other `Value` flavour falls
/// back to its `Debug` rendering so the order is at least deterministic.
pub(super) fn yaml_key_sort_key(v: &serde_yaml_ng::Value) -> String {
    match v {
        serde_yaml_ng::Value::String(s) => s.clone(),
        other => format!("{:?}", other),
    }
}

/// Print the artifact size report if `report_sizes` is enabled in config.
pub fn run_report_sizes(ctx: &mut Context, config: &Config, log: &StageLogger) {
    if config.report_sizes.unwrap_or(false) {
        anodizer_core::artifact::print_size_report(&mut ctx.artifacts, log);
    }
}

/// Write `dist/metadata.json` from the current context's resolved
/// release variables (`tag`, `previous_tag`, `version`, `commit`,
/// `date`, `release_url`, host `runtime`) and return the path it
/// landed at.
///
/// The output directory is taken from `ctx.config.dist`, NOT the
/// `config` parameter. Per-crate publish-only re-anchors `ctx.config.dist`
/// onto the per-crate `dist/<crate>/` subdir while still threading the
/// flat-root `config` through; the release stage's existence gate reads
/// `ctx.config.dist/metadata.json`, so the file must land there. For the
/// full-release callers `ctx.config.dist == config.dist`, so this is
/// behaviour-preserving for them.
///
/// Writes the metadata file. Does **not** register the file
/// as an artifact — callers that need the registry entry (full release
/// post-pipeline) add it; callers that already rehydrated the registry
/// (per-crate publish-only) reuse the existing entry.
pub fn write_metadata_json(
    ctx: &Context,
    config: &Config,
    log: &StageLogger,
) -> Result<std::path::PathBuf> {
    let dist = &ctx.config.dist;
    std::fs::create_dir_all(dist)
        .with_context(|| format!("failed to create dist directory: {}", dist.display()))?;

    let metadata_path = dist.join(anodizer_core::dist::METADATA_JSON);
    let goos = anodizer_core::context::map_os_to_goos(std::env::consts::OS);
    let goarch = anodizer_core::context::map_arch_to_goarch(std::env::consts::ARCH);

    let tag = ctx.template_vars().get("Tag").cloned().unwrap_or_default();
    let previous_tag = ctx
        .template_vars()
        .get("PreviousTag")
        .cloned()
        .unwrap_or_default();
    let version = ctx.version();
    let commit = ctx
        .template_vars()
        .get("FullCommit")
        .cloned()
        .unwrap_or_default();
    let date = ctx.template_vars().get("Date").cloned().unwrap_or_default();
    // Same source as the `{{ ReleaseURL }}` template var the announce /
    // webhook stages render: the release stage's authoritative `html_url`
    // (or its derived default). Reading the var — instead of re-composing
    // the URL here — keeps the two surfaces from ever drifting.
    let release_url = ctx
        .template_vars()
        .get("ReleaseURL")
        .cloned()
        .unwrap_or_default();

    let project_metadata = serde_json::json!({
        "project_name": config.project_name,
        "tag": tag,
        "previous_tag": previous_tag,
        "version": version,
        "commit": commit,
        "date": date,
        "release_url": release_url,
        "runtime": {
            "goos": goos,
            "goarch": goarch,
        }
    });

    let json_str = serde_json::to_string_pretty(&project_metadata)
        .context("failed to serialize project metadata JSON")?;
    std::fs::write(&metadata_path, &json_str)
        .with_context(|| format!("failed to write {}", metadata_path.display()))?;
    log.status(&format!("wrote {}", metadata_path.display()));

    Ok(metadata_path)
}

/// Compile-time coupling to the determinism harness's aggregate registry: the
/// `artifacts.json` manifest this function writes is recognized by
/// `anodizer_core::determinism::ArtifactsManifest`, whose `id()` is this const.
/// Referencing it welds the producer to the registry entry so neither can be
/// renamed without breaking the build (mirrors the combined-checksums coupling
/// in `anodizer_stage_checksum`).
const _: &str = anodizer_core::determinism::ARTIFACTS_MANIFEST_AGGREGATE_ID;

/// Write `dist/metadata.json` and `dist/artifacts.json` and apply the
/// configured `metadata.mod_timestamp` to both files.
///
/// Writes the metadata + artifacts files. Registers
/// `metadata.json` as an artifact so downstream stages can pick it up.
pub fn write_metadata_and_artifacts(
    ctx: &mut Context,
    config: &Config,
    log: &StageLogger,
) -> Result<()> {
    // Co-locate artifacts.json with metadata.json. `write_metadata_json`
    // anchors on `ctx.config.dist`; mirror that here so the sibling pair
    // never splits across two directories.
    let dist = ctx.config.dist.clone();
    let metadata_path = write_metadata_json(ctx, config, log)?;

    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: ArtifactKind::Metadata,
        name: anodizer_core::dist::METADATA_JSON.to_string(),
        path: metadata_path.clone(),
        target: None,
        crate_name: config.project_name.clone(),
        metadata: Default::default(),
        size: None,
    });

    let artifacts_path = dist.join(anodizer_core::dist::ARTIFACTS_JSON);
    let artifacts_json = ctx
        .artifacts
        .to_artifacts_json()
        .context("failed to serialize artifact list")?;
    let json_str = serde_json::to_string_pretty(&artifacts_json)
        .context("failed to serialize artifacts JSON")?;
    std::fs::write(&artifacts_path, &json_str)
        .with_context(|| format!("failed to write {}", artifacts_path.display()))?;
    log.status(&format!("wrote {}", artifacts_path.display()));

    if let Some(ref meta) = config.metadata
        && let Some(ref ts_tmpl) = meta.mod_timestamp
    {
        let rendered = ctx
            .render_template(ts_tmpl)
            .context("failed to render metadata.mod_timestamp template")?;
        if !rendered.is_empty() {
            let mtime = anodizer_core::util::parse_mod_timestamp(&rendered)
                .with_context(|| format!("invalid metadata.mod_timestamp value: {:?}", rendered))?;
            anodizer_core::util::set_file_mtime(&metadata_path, mtime)?;
            anodizer_core::util::set_file_mtime(&artifacts_path, mtime)?;
            log.status(&format!(
                "set mtime on metadata.json and artifacts.json to {}",
                rendered
            ));
        }
    }

    Ok(())
}
