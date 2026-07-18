use super::*;

/// A submitter moderation-queue advisory paired with the dispatch publisher
/// identity that produced it. The CLI filters by [`SubmitterAdvisory::publisher`]
/// so an advisory for a publisher deselected by `--skip` / `--publishers`
/// (e.g. `chocolatey` under a `--publishers npm` run) is suppressed instead of
/// emitted as noise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitterAdvisory {
    /// Dispatch publisher name, matching the string
    /// [`crate::context::Context::publisher_deselected`] tests: `chocolatey`,
    /// `winget`, or `upstream-aur` (the AUR-source publisher's dispatch name).
    /// The CLI keys its deselection predicate on this value.
    pub publisher: String,
    /// The verbose advisory line surfaced to the operator.
    pub message: String,
}

/// One advisory per publisher configured with `required: true` whose group is
/// Submitter (chocolatey, winget, aur_source), each tagged with its dispatch
/// publisher identity so the CLI can suppress advisories for deselected
/// publishers.
///
/// `required: true` on a submitter still fails the release when the submission
/// itself fails (it feeds `required_failures()`), but the external moderation
/// outcome resolves after the release run and cannot be gated on. The advisory
/// is non-fatal and clarifies which half of the semantics applies. Cargo is
/// excluded: its default is already `required: true` and the message would be
/// noise.
///
/// Covers all three publish axes — `crates[].publish`,
/// `workspaces[].crates[].publish`, and `defaults.publish` (via
/// [`for_each_crate_publish`]) — plus the top-level `aur_sources:` list.
///
/// Pure: this returns the advisories without emitting them. The CLI surfaces
/// them through `StageLogger::verbose` (the `--verbose`-gated register), so
/// they stay hidden at the default log level — see
/// `pipeline::load_config_logged`.
pub fn submitter_required_warnings(config: &Config) -> Vec<SubmitterAdvisory> {
    fn advisory(location: &str, name: &str, publisher: &str) -> SubmitterAdvisory {
        SubmitterAdvisory {
            publisher: publisher.to_string(),
            message: format!(
                "{location}: publisher '{name}' submits to an external moderation queue; \
                 `required: true` fails the release when the submission itself fails, \
                 but the eventual moderation outcome happens outside the release run \
                 and cannot be gated."
            ),
        }
    }

    let mut warnings = Vec::new();

    for_each_crate_publish(config, |axis, publish| {
        let loc = axis.location();
        if publish.chocolatey().and_then(|c| c.required) == Some(true) {
            warnings.push(advisory(&loc, "chocolatey", "chocolatey"));
        }
        if publish.winget().and_then(|w| w.required) == Some(true) {
            warnings.push(advisory(&loc, "winget", "winget"));
        }
        if publish.aur_source().and_then(|a| a.required) == Some(true) {
            // The AUR-source publisher dispatches under the name `upstream-aur`
            // (`AurSourcePublisher::PUBLISHER_NAME`); key the advisory on that so
            // the CLI's `publisher_deselected("upstream-aur")` filter matches.
            warnings.push(advisory(&loc, "aur_source", "upstream-aur"));
        }
    });

    // Top-level aur_sources list (not nested under publish:) — no crate axis,
    // distinguish via the index in the list so two top-level entries collide cleanly.
    if let Some(ref sources) = config.aur_sources {
        for (idx, src) in sources.iter().enumerate() {
            if src.required == Some(true) {
                let loc = format!("top-level aur_sources[{idx}]");
                warnings.push(advisory(&loc, "aur_source", "upstream-aur"));
            }
        }
    }

    warnings
}

/// No-op preserved for API stability; the legacy `format:` and `builds:`
/// folds happen inline in `<ArchiveConfig as Deserialize>::deserialize` and
/// `<FormatOverride as Deserialize>::deserialize`. Emits no warning of its
/// own — every alias hit was already announced at deserialize time.
///
pub fn apply_archive_legacy_aliases(_config: &mut Config) {
    // Intentionally empty — see Deserialize impls.
}

/// Reject the legacy V1 `dockers:` block at config-load time with a
/// clear migration error.
///
/// anodizer is V2-only by design: it implements `dockers_v2:` and the
/// associated multi-arch buildx flow, but does not ship the V1
/// `dockers: -> dockerfile + image_templates` pipe. Without this check the
/// top-level `Config` struct's `deny_unknown_fields` would emit a generic
/// "unknown field `dockers`" message that doesn't tell the user how to
/// migrate. This explicit error names the field, points at `dockers_v2:`,
/// and references the rationale.
///
pub fn validate_no_docker_v1(raw_yaml: &serde_yaml_ng::Value) -> Result<(), String> {
    if raw_yaml.get("dockers").is_some() {
        return Err(
            "config: legacy GoReleaser `dockers:` block is not supported — anodizer ships \
             dockers_v2: only (multi-arch buildx flow). Port the config to `dockers_v2:` per \
             https://anodize.dev/docs/migration/docker.html."
                .to_string(),
        );
    }
    Ok(())
}

/// Emit a `tracing::warn!` for each `publish.homebrew:` (Homebrew Formula)
/// occurrence in the loaded config. The upstream deprecated the
/// Formula publisher in favour of `homebrew_casks:`; anodizer mirrors the
/// upstream deprecation so users following the change-log see the
/// same migration prompt.
///
/// Covers three placement axes (matching how `publish.homebrew` may appear):
///   * `crates[].publish.homebrew`
///   * `workspaces[].crates[].publish.homebrew`
///   * `defaults.publish.homebrew`
///
/// There is no top-level `homebrew:` or `brews:` field on anodizer's
/// `Config` — only `homebrew_casks:` lives at the top level — so this
/// function does not need a top-level scan.
pub fn warn_on_legacy_homebrew_formula(config: &Config) {
    for msg in legacy_homebrew_formula_warnings(config) {
        tracing::warn!("{}", msg);
    }
}

/// Pure helper: returns the warning strings without emitting them.
/// Exposed for tests; production callers use
/// [`warn_on_legacy_homebrew_formula`].
pub(crate) fn legacy_homebrew_formula_warnings(config: &Config) -> Vec<String> {
    fn formula_warning(location: &str) -> String {
        format!(
            "DEPRECATION: {location}: publish.homebrew (Homebrew Formula) is deprecated upstream \
             in GoReleaser v2.16; migrate to homebrew_casks. Cask is now the canonical Homebrew \
             distribution channel for pre-compiled binaries. See \
             https://anodize.dev/docs/publish/homebrew-casks/ for migration."
        )
    }

    let mut warnings = Vec::new();

    for_each_crate_publish(config, |axis, publish| {
        if publish.homebrew().is_some() {
            warnings.push(formula_warning(&axis.location()));
        }
    });

    warnings
}

/// Fold the deprecated `snapshot.name_template` alias into `version_template`.
/// Serde already accepts both spellings via `#[serde(alias = "name_template")]`,
/// so this function only needs to emit the deprecation warning when the
/// raw YAML key was the legacy one.
///
/// Because serde collapses the two spellings to a single field on parse, we
/// lose the information about which key the user wrote. This function
/// therefore consults the raw YAML pre-parse value (when supplied) to decide.
pub fn warn_on_legacy_snapshot_name_template(raw_yaml: &serde_yaml_ng::Value) {
    if let Some(snap) = raw_yaml.get("snapshot")
        && snap.get("name_template").is_some()
    {
        tracing::warn!(
            "DEPRECATION: snapshot.name_template is deprecated; use \
             snapshot.version_template instead. Both spellings are accepted \
             but the legacy key will be removed in a future release."
        );
    }
}

/// Emit a one-time deprecation warning when a config uses the legacy
/// `furies:` top-level key. Serde transparently folds `furies:` into
/// `gemfury:` via `#[serde(alias)]`, so this function consults the raw YAML
/// pre-parse value to detect the legacy spelling.
///
/// The `furies → gemfury` rename messaging.
pub fn warn_on_legacy_furies_alias(raw_yaml: &serde_yaml_ng::Value) {
    if raw_yaml.get("furies").is_some() {
        tracing::warn!(
            "DEPRECATION: the top-level `furies:` config key is deprecated since GoReleaser \
             Pro v2.14; rename it to `gemfury:`. Both spellings are accepted but the legacy \
             key will be removed in a future release."
        );
    }
}

/// Emit a one-time deprecation warning for each nfpm config object that uses
/// the legacy `builds:` key. Serde transparently folds `builds:` into `ids:`
/// via `#[serde(alias = "builds")]` on [`NfpmConfig::ids`], so this function
/// consults the raw YAML pre-parse value to detect the legacy spelling that the
/// typed parse would otherwise erase.
///
/// The deprecated `NFPM.Builds` field (use `ids` instead).
///
/// nfpm config objects appear under the key `nfpm` or `nfpms` (a single map or
/// a sequence of maps) at multiple nesting depths — top-level, under
/// `defaults:`, under each `crates[]` entry, and under each
/// `workspaces[].crates[]` entry. Rather than enumerate every path, this walks
/// the tree recursively and inspects a node as an nfpm config only when it is
/// the value of an `nfpm:`/`nfpms:` key, so an unrelated `builds:` key
/// elsewhere (e.g. archives) is not double-counted.
pub fn warn_on_legacy_nfpm_builds(raw_yaml: &serde_yaml_ng::Value) {
    fn warn_for_nfpm_value(value: &serde_yaml_ng::Value) {
        match value {
            serde_yaml_ng::Value::Mapping(_) => {
                if value.get("builds").is_some() {
                    tracing::warn!(
                        "DEPRECATION: nfpm `builds:` is deprecated; use `ids:` instead. \
                         Both spellings are accepted but the legacy key will be removed in \
                         a future release."
                    );
                }
            }
            serde_yaml_ng::Value::Sequence(items) => {
                for item in items {
                    warn_for_nfpm_value(item);
                }
            }
            _ => {}
        }
    }

    fn descend(value: &serde_yaml_ng::Value) {
        match value {
            serde_yaml_ng::Value::Mapping(map) => {
                for (key, child) in map {
                    if matches!(key.as_str(), Some("nfpm") | Some("nfpms")) {
                        warn_for_nfpm_value(child);
                    }
                    descend(child);
                }
            }
            serde_yaml_ng::Value::Sequence(items) => {
                for item in items {
                    descend(item);
                }
            }
            _ => {}
        }
    }

    descend(raw_yaml);
}

/// Emit a one-time deprecation warning for each block that carries the legacy
/// `disable:` spelling of the canonical `skip:` field. Many config blocks
/// (`release`, `changelog`, `snapcraft`, the docker / installer / packager
/// blocks, …) accept `disable:` via `#[serde(alias = "disable")]` for
/// back-compat with imported configs; serde folds the alias into
/// `skip` on parse, erasing which spelling the user wrote. This helper
/// consults the raw YAML pre-parse value so porting users get a migration
/// prompt pointing at the canonical `skip:`.
///
/// Detection is allow-listed by enclosing block key, NOT a blind tree walk,
/// because free-form string-keyed maps would otherwise produce false
/// positives:
///   * Free-form string-keyed maps (`variables`, `derived_metadata`,
///     `build_args`, `labels`, `annotations`, `env`, header maps, …) let a
///     user legitimately name a key `disable`. Matching only when the key's
///     immediate enclosing block is allow-listed skips those — the nearest
///     named ancestor of such a key is the map's own key (e.g. `build_args`),
///     never an allow-listed block.
///
/// Axis-agnostic: the enclosing block key is identical whether the block sits
/// at the top level, under `defaults.<block>`, under `crates[].<block>`, or
/// under `workspaces[].crates[].<block>`, so a single nearest-named-ancestor
/// rule covers every placement.
pub fn warn_on_legacy_disable_alias(raw_yaml: &serde_yaml_ng::Value) {
    for msg in legacy_disable_alias_warnings(raw_yaml) {
        tracing::warn!("{}", msg);
    }
}

/// Pure helper: returns one warning string per offending `disable:` key,
/// each naming the YAML path to the key. Exposed for tests; production callers
/// use [`warn_on_legacy_disable_alias`].
pub(crate) fn legacy_disable_alias_warnings(raw_yaml: &serde_yaml_ng::Value) -> Vec<String> {
    // Block key names whose struct exposes `skip` with `#[serde(alias =
    // "disable")]`. Resolved from the field's serde key on its parent (see the
    // `alias = "disable"` sites in core). `makeselfs` (top-level) and
    // `makeselves` (defaults.) both map to MakeselfConfig, so both are listed;
    // `gemfury` and its legacy `furies` alias both map to GemFuryConfig.
    const ALLOWLIST: &[&str] = &[
        "mcp",
        "makeselfs",
        "makeselves",
        "install_scripts",
        "appimages",
        "msis",
        "pkgs",
        "nsis",
        "dockerhub",
        "release",
        "dockers_v2",
        "docker_v2",
        "changelog",
        "snapcrafts",
        "npms",
        "gemfury",
        "furies",
        "pypis",
        "homebrew_cores",
        "publishers",
        "sboms",
        "aur",
        "aur_source",
        "aur_sources",
        "blobs",
        "docker_digest",
        "checksum",
        "flatpaks",
    ];

    fn disable_warning(path: &str) -> String {
        format!(
            "DEPRECATION: {path}: legacy `disable:` is deprecated; rename it to `skip:`. \
             Both spellings are accepted but the legacy key will be removed in a future release."
        )
    }

    // `enclosing_block`: the nearest named (non-list-index) ancestor key — the
    // block the `disable:` key belongs to. Only warn when it is allow-listed.
    fn descend(
        value: &serde_yaml_ng::Value,
        path: &str,
        enclosing_block: Option<&str>,
        warnings: &mut Vec<String>,
    ) {
        match value {
            serde_yaml_ng::Value::Mapping(map) => {
                for (key, child) in map {
                    let Some(key) = key.as_str() else { continue };
                    let child_path = if path.is_empty() {
                        key.to_string()
                    } else {
                        format!("{path}.{key}")
                    };
                    if key == "disable"
                        && enclosing_block.is_some_and(|block| ALLOWLIST.contains(&block))
                    {
                        warnings.push(disable_warning(&child_path));
                    }
                    descend(child, &child_path, Some(key), warnings);
                }
            }
            serde_yaml_ng::Value::Sequence(items) => {
                for (idx, item) in items.iter().enumerate() {
                    let item_path = format!("{path}[{idx}]");
                    // A list index is not a named ancestor: keep the enclosing
                    // block (the list's own key) so e.g. `snapcrafts[0].disable`
                    // still resolves to the `snapcrafts` block.
                    descend(item, &item_path, enclosing_block, warnings);
                }
            }
            _ => {}
        }
    }

    let mut warnings = Vec::new();
    descend(raw_yaml, "", None, &mut warnings);
    warnings
}

/// Reject the legacy nested `mcp.github:` block with a
/// clear migration error.
///
/// The registry metadata that used to live under
/// `mcp.github:` (repository owner/name/url) to the top-level `mcp:` block
/// (canonical surface: `mcp.repository:`, `mcp.name:`, etc.). Anodizer
/// never carried the nested shim — its `McpConfig` has `deny_unknown_fields`
/// so the key would otherwise produce a generic "unknown field" message.
/// This pre-parse check intercepts the legacy spelling so the user sees a
/// migration pointer rather than a schema-shape error.
pub fn validate_no_mcp_github(raw_yaml: &serde_yaml_ng::Value) -> Result<(), String> {
    if raw_yaml.get("mcp").and_then(|m| m.get("github")).is_some() {
        return Err(
            "config: nested `mcp.github:` block is not supported — anodizer mirrors GoReleaser \
             v2.13.1+ where registry metadata moved to top-level `mcp:` fields (`mcp.name`, \
             `mcp.repository.url`, `mcp.repository.source`). Port the nested keys to the \
             canonical surface."
                .to_string(),
        );
    }
    Ok(())
}

/// Emit a one-time deprecation warning for each `dockers_v2[].retry:` or
/// `docker_manifests[].retry:` block at config-load time. The per-pipe
/// `retry:` field is the legacy shape (retry handling moved to
/// the top-level `retry:` block); the per-pipe value is still honored at
/// resolve-time (see `stage-docker::resolve_retry_params`) but a top-level
/// `retry:` is the canonical surface for retry policy. Warning fires once
/// per occurrence so users porting from older configs see a clear
/// pointer at load time without waiting for the docker pipe to execute.
pub fn warn_on_legacy_docker_retry(config: &Config) {
    for msg in legacy_docker_retry_warnings(config) {
        tracing::warn!("{}", msg);
    }
}

/// Pure helper: returns the warning strings without emitting them. Exposed
/// for tests; production callers use [`warn_on_legacy_docker_retry`].
pub(crate) fn legacy_docker_retry_warnings(config: &Config) -> Vec<String> {
    fn pipe_warning(location: &str, kind: &str) -> String {
        format!(
            "DEPRECATION: {location}: nested `{kind}.retry:` is deprecated since GoReleaser \
             v2.15.3; move retry settings to the top-level `retry:` block. The per-pipe \
             value still wins at resolve time for back-compat, but the legacy spelling will \
             be removed in a future release."
        )
    }

    let mut warnings = Vec::new();

    let scan_crate = |krate: &CrateConfig, prefix: &str, warnings: &mut Vec<String>| {
        if let Some(ref v2) = krate.dockers_v2 {
            for (i, cfg) in v2.iter().enumerate() {
                if cfg.retry.is_some() {
                    warnings.push(pipe_warning(
                        &format!("{prefix}.dockers_v2[{i}]"),
                        "dockers_v2",
                    ));
                }
            }
        }
        if let Some(ref manifests) = krate.docker_manifests {
            for (i, cfg) in manifests.iter().enumerate() {
                if cfg.retry.is_some() {
                    warnings.push(pipe_warning(
                        &format!("{prefix}.docker_manifests[{i}]"),
                        "docker_manifests",
                    ));
                }
            }
        }
    };

    for krate in &config.crates {
        scan_crate(krate, &format!("crates[{}]", krate.name), &mut warnings);
    }

    if let Some(ref workspaces) = config.workspaces {
        for ws in workspaces {
            for krate in &ws.crates {
                scan_crate(
                    krate,
                    &format!("workspaces[{}].crates[{}]", ws.name, krate.name),
                    &mut warnings,
                );
            }
        }
    }

    if let Some(ref defaults) = config.defaults
        && let Some(ref v2) = defaults.dockers_v2
        && v2.retry.is_some()
    {
        warnings.push(pipe_warning("defaults.dockers_v2", "dockers_v2"));
    }

    warnings
}

/// Fold the deprecated singular Homebrew Cask fields into their canonical
/// plural lists and emit a one-time deprecation warning per folded field:
///
/// - `binary: <name>` → [`HomebrewCaskConfig::binaries`] (the upstream
///   renamed `binary:` to `binaries:`).
/// - `manpage: <page>` → [`HomebrewCaskConfig::manpages`].
///
/// anodizer accepts both spellings so imported configs keep parsing.
/// The captured values are moved out of [`HomebrewCaskConfig::legacy_binary`]
/// and [`HomebrewCaskConfig::legacy_manpage`] so downstream code only ever
/// reads the canonical plural fields.
///
/// The two folds use different insertion order: a legacy
/// `binary` is **prepended** to `binaries` so any explicit `binaries:` ordering
/// is preserved at the tail, whereas a legacy `manpage` is **appended** to
/// `manpages` (the cask renderer does
/// `brew.Manpages = append(brew.Manpages, brew.Manpage)`).
///
/// The fold runs across every config mode — top-level `homebrew_casks`,
/// per-crate `publish.homebrew_cask`, `workspaces[].crates[].publish`, and
/// `defaults.publish`.
pub fn apply_homebrew_cask_legacy_singulars(config: &mut Config) {
    /// Fold both deprecated singular fields (`binary:` → `binaries`,
    /// `manpage:` → `manpages`) on one cask, returning a warning per folded
    /// field. The singular `binary` is prepended to `binaries` so an explicit
    /// `binaries[0]` ordering is preserved at the tail; the singular `manpage`
    /// is appended to `manpages`.
    fn fold_one(location: &str, cask: &mut HomebrewCaskConfig) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(legacy) = cask.legacy_binary.take() {
            let entry = HomebrewCaskBinary::Name(legacy.clone());
            match cask.binaries {
                Some(ref mut list) => list.insert(0, entry),
                None => cask.binaries = Some(vec![entry]),
            }
            warnings.push(format!(
                "DEPRECATION: {location}: singular `binary: {legacy}` is deprecated since \
                 GoReleaser v2.12.6; use the plural `binaries: [{legacy}]` form. The legacy \
                 value has been folded into binaries[0]."
            ));
        }
        if let Some(legacy) = cask.legacy_manpage.take() {
            match cask.manpages {
                Some(ref mut list) => list.push(legacy.clone()),
                None => cask.manpages = Some(vec![legacy.clone()]),
            }
            warnings.push(format!(
                "DEPRECATION: {location}: singular `manpage: {legacy}` is deprecated; \
                 use the plural `manpages: [{legacy}]` form. The legacy value has been \
                 folded into manpages."
            ));
        }
        warnings
    }

    let mut warnings = Vec::new();

    // Top-level homebrew_casks list (not nested under publish:) — not a
    // publish axis, so it is scanned separately from the visitor.
    if let Some(ref mut casks) = config.homebrew_casks {
        for (i, cask) in casks.iter_mut().enumerate() {
            warnings.extend(fold_one(&format!("homebrew_casks[{i}]"), cask));
        }
    }

    for_each_crate_publish_mut(config, |axis, mut publish| {
        if let Some(cask) = publish.homebrew_cask_mut() {
            warnings.extend(fold_one(&axis.homebrew_cask_location(), cask));
        }
    });

    for msg in warnings {
        tracing::warn!("{}", msg);
    }
}
