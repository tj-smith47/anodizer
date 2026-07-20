use super::*;

// ---------------------------------------------------------------------------
// generate_manifest
// ---------------------------------------------------------------------------

/// Optional extended fields for manifest generation.
#[derive(Default)]
pub(crate) struct ManifestOptions<'a> {
    /// Explicit homepage URL.  Falls back to the GitHub release URL when available.
    pub(crate) homepage: Option<&'a str>,
    /// GitHub owner/name for default homepage fallback (e.g. "owner/repo").
    pub(crate) github_slug: Option<String>,
    /// Data paths persisted between updates.
    pub(crate) persist: Option<&'a [String]>,
    /// Application dependencies.
    pub(crate) depends: Option<&'a [String]>,
    /// Commands to run before installation.
    pub(crate) pre_install: Option<&'a [String]>,
    /// Commands to run after installation.
    pub(crate) post_install: Option<&'a [String]>,
    /// Start menu shortcuts.
    pub(crate) shortcuts: Option<&'a [Vec<String>]>,
    /// Binary names (without `.exe` extension) to use in the `bin` field.
    /// When set, these are used instead of deriving from the manifest name.
    /// Multiple entries produce a JSON array in the `bin` field.
    pub(crate) bin: Option<&'a [String]>,
    /// `checkver` strategy. When `Some`, emitted verbatim (`"github"` or a
    /// homepage regex). When `None`, the key is omitted.
    pub(crate) checkver: Option<String>,
    /// How `autoupdate.hash` should be resolved for each architecture. When
    /// `None`, no `autoupdate` block is emitted (and no `checkver` either,
    /// since a checkver without autoupdate is a dead half-manifest).
    pub(crate) autoupdate_hash: Option<AutoupdateHash>,
}

/// How scoop's `autoupdate.architecture.<arch>.hash` is resolved, mirroring
/// what anodizer actually publishes (see the checksum stage):
/// - [`AutoupdateHash::UrlSidecar`] when split mode emits per-asset
///   `<asset>.sha256` sidecars — scoop fetches `$url.sha256`.
/// - [`AutoupdateHash::ChecksumsRegex`] when the combined `checksums.txt` is
///   the only hash source — scoop fetches that file and extracts the line for
///   the asset via a regex over `<sha>␠<basename>`.
#[derive(Clone)]
pub(crate) enum AutoupdateHash {
    /// Per-asset sidecar: `hash: { "url": "$url.<suffix>" }`. `suffix` is the
    /// checksum-stage sidecar extension — the effective `checksum.algorithm`
    /// (`sha256`, `sha512`, `blake3`, …) — NOT a hardcoded `sha256`. Scoop's
    /// `$url` resolves to the per-arch asset URL, so this is only valid when
    /// the real sidecar is named `<asset>.<suffix>`.
    UrlSidecar { suffix: String },
    /// Combined checksums file. `url_template` carries the file URL with the
    /// version already substituted by `$version`.
    ChecksumsRegex { url_template: String },
}

/// Reject a `use:` value scoop cannot honor. Scoop installs by unpacking an
/// archive (`.zip`/`.tar.gz`/`.tgz`); it has no mechanism to run an installer,
/// so `use: msi`/`nsis`/`wix`/`exe` would render a structurally-valid bucket
/// manifest whose `architecture.<arch>.url` points at a payload `scoop install`
/// cannot execute. Fail loud at selection rather than ship that broken-silent
/// manifest. `archive` (and any unrecognized/`None` value, which defaults to
/// archive) is accepted.
pub(crate) fn reject_unsupported_use(use_artifact: Option<&str>, crate_name: &str) -> Result<()> {
    if let Some(value @ ("msi" | "nsis" | "wix" | "exe")) = use_artifact {
        anyhow::bail!(
            "scoop: `use: {value}` is unsupported for crate '{crate_name}' — scoop installs \
             from archives only (it unzips, it cannot run an installer). Ship the windows \
             `.zip` archive and use the default `use: archive`, or publish the {value} \
             installer through a publisher that runs it (winget / chocolatey).",
        );
    }
    Ok(())
}

/// Replace every occurrence of the concrete `version` in `s` with scoop's
/// `$version` placeholder, producing an autoupdate-ready template. The version
/// appears in both the release tag path and the asset filename, so all
/// occurrences are substituted (the standard scoop autoupdate convention).
///
/// Naive global replace: only the tag-path and asset-filename occurrences of
/// the version are intended. A version string that coincidentally appears
/// elsewhere in the URL (e.g. a host or query segment equal to the version)
/// would over-substitute. In practice GitHub release URLs carry the version
/// only in the `/download/<tag>/` and asset-name segments, so the naive
/// replace matches scoop's own `$version` convention; anchoring is not worth
/// the false-negative risk of a stricter matcher.
///
/// When `version` is empty the input is returned unchanged — an empty needle
/// would otherwise splice `$version` between every byte.
fn substitute_version(s: &str, version: &str) -> String {
    if version.is_empty() {
        return s.to_string();
    }
    s.replace(version, "$version")
}

/// A single architecture entry for the Scoop manifest.
pub(crate) struct ArchEntry {
    /// Scoop architecture key: "64bit", "32bit", or "arm64".
    pub(crate) scoop_arch: String,
    pub(crate) url: String,
    pub(crate) hash: String,
    /// When the archive wraps contents in a top-level directory, this holds that
    /// directory name.  Bin entries will be prefixed with it (e.g. `dir/bin.exe`).
    pub(crate) wrap_in_directory: Option<String>,
}

/// Generate a single-architecture Scoop JSON manifest string for a Windows
/// binary. A thin wrapper over [`generate_manifest_with_opts`] that the unit
/// tests use to exercise manifest shape without assembling an `ArchEntry` set;
/// the production publish path always renders through
/// [`generate_manifest_with_opts`] directly.
#[cfg(test)]
pub(crate) fn generate_manifest(
    name: &str,
    version: &str,
    url: &str,
    hash: &str,
    description: &str,
    license: &str,
) -> Result<String> {
    let entries = vec![ArchEntry {
        scoop_arch: "64bit".to_string(),
        url: url.to_string(),
        hash: hash.to_string(),
        wrap_in_directory: None,
    }];
    generate_manifest_with_opts(
        name,
        version,
        &entries,
        description,
        license,
        &ManifestOptions::default(),
    )
}

/// The effective checksum settings that drive scoop's autoupdate.hash, merged
/// once (per-crate override → `defaults.checksum` → built-in default) so the
/// split / algorithm / name_template trio cannot disagree across the two
/// branches of [`resolve_autoupdate_hash`].
struct EffectiveChecksumConfig {
    split: bool,
    algorithm: String,
    name_template: Option<String>,
}

impl EffectiveChecksumConfig {
    fn resolve(ctx: &Context, crate_cfg: &anodizer_core::config::CrateConfig) -> Self {
        use anodizer_core::config::ChecksumConfig;

        let crate_cksum = crate_cfg.checksum.as_ref();
        let global_cksum = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.checksum.as_ref());
        let pick = |f: &dyn Fn(&ChecksumConfig) -> Option<String>| -> Option<String> {
            crate_cksum.and_then(f).or_else(|| global_cksum.and_then(f))
        };

        EffectiveChecksumConfig {
            split: crate_cksum
                .and_then(|c| c.split)
                .or_else(|| global_cksum.and_then(|c| c.split))
                .unwrap_or(false),
            algorithm: pick(&|c| c.algorithm.clone())
                .unwrap_or_else(|| ChecksumConfig::DEFAULT_ALGORITHM.to_string()),
            name_template: pick(&|c| c.name_template.clone()),
        }
    }
}

/// Resolve how scoop's `autoupdate.hash` should be derived for this crate,
/// reading the crate's effective checksum config (per-crate override falling
/// back to `defaults.checksum`).
///
/// - **split mode** (`checksum.split: true`): anodizer emits a per-asset
///   `<asset>.<algorithm>` sidecar, so scoop fetches `$url.<algorithm>` — the
///   suffix is the EFFECTIVE checksum algorithm (`sha256`/`sha512`/`blake3`/…),
///   never a hardcoded `.sha256`. A custom split `name_template` is honored
///   only when it renders to exactly `<asset>.<suffix>` (so `$url.<suffix>`
///   resolves); any other shape hard-fails rather than emit a 404-ing URL.
/// - **combined mode** (default): the only hash source is the single
///   `checksums.txt`, so scoop fetches that file (URL templated with
///   `$version`) and extracts the per-asset line via a regex.
///
/// Hard-fails when:
/// - split mode uses a custom `name_template` whose sidecar URL is not the
///   `<asset>.<suffix>` shape scoop's `$url.<suffix>` requires, or
/// - combined mode has no release asset URL to anchor the checksums-file URL.
///   Emitting an autoupdate URL that 404s would silently break every future
///   auto-bump — the spec forbids the hand-derived value that drifts.
pub(crate) fn resolve_autoupdate_hash(
    ctx: &Context,
    crate_cfg: &anodizer_core::config::CrateConfig,
    crate_name: &str,
    version: &str,
    arch_entries: &[ArchEntry],
) -> Result<AutoupdateHash> {
    use anodizer_core::config::ChecksumConfig;

    let effective = EffectiveChecksumConfig::resolve(ctx, crate_cfg);

    if effective.split {
        let suffix = resolve_sidecar_suffix(
            ctx,
            crate_name,
            &effective.algorithm,
            effective.name_template.as_deref(),
            arch_entries,
        )?;
        return Ok(AutoupdateHash::UrlSidecar { suffix });
    }

    // Combined mode: build the checksums-file URL by replacing the asset
    // basename in a real release URL with the rendered checksums filename,
    // then substituting the version with `$version`.
    let sample_url = arch_entries
        .first()
        .map(|e| e.url.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "scoop: cannot build autoupdate.hash for crate '{}': no release \
                 asset URL is available to anchor the combined checksums-file URL. \
                 Either enable per-asset sidecars (`checksum.split: true`) or \
                 ensure a Windows archive artifact carries its release `url` metadata.",
                crate_name
            )
        })?;

    // Resolve the combined checksums filename via the same template the
    // checksum stage uses, then map the concrete version to `$version`.
    let name_template = effective
        .name_template
        .clone()
        .unwrap_or_else(|| ChecksumConfig::DEFAULT_NAME_TEMPLATE.to_string());
    let checksums_name = ctx.render_template(&name_template).with_context(|| {
        format!("scoop: render checksums name template for autoupdate ('{crate_name}')")
    })?;

    // Replace the asset filename (last path segment) with the checksums file.
    let base = sample_url.rsplit_once('/').map(|(b, _)| b).unwrap_or("");
    let checksums_url = if base.is_empty() {
        checksums_name.clone()
    } else {
        format!("{}/{}", base, checksums_name)
    };
    let url_template = substitute_version(&checksums_url, version);

    Ok(AutoupdateHash::ChecksumsRegex { url_template })
}

/// Derive the per-asset sidecar suffix scoop appends as `$url.<suffix>`.
///
/// Mirrors the checksum stage's split-mode naming (`resolve_sidecar_path`):
/// the default sidecar is `<asset>.<algorithm>`, so the suffix is the
/// algorithm. With a custom split `name_template`, the sidecar name is only an
/// `$url.<suffix>` form when the template renders to `<asset>.<suffix>` for an
/// arbitrary asset; this probes that by rendering with a sentinel
/// `ArtifactName` and confirming the result is `<sentinel>.<suffix>`. Anything
/// else (the asset name embedded mid-string, a different prefix, …) cannot be
/// expressed as `$url.<suffix>` and hard-fails — never a guessed, 404-ing URL.
///
/// One further trap: a template may embed `{{ ArtifactExt }}` in the *suffix*
/// (e.g. `{{ ArtifactName }}.{{ ArtifactExt }}.sha256`). Scoop's `$url.<suffix>`
/// is a single static string, so a suffix that varies with the asset extension
/// is only sound when every windows asset shares one extension. If the assets
/// are not uniformly `.zip` (scoop also accepts `.tar.gz`/`.tgz`), the suffix
/// would 404 for the non-matching asset, so this hard-fails rather than bake a
/// per-asset-varying extension into a static suffix.
fn resolve_sidecar_suffix(
    ctx: &Context,
    crate_name: &str,
    algorithm: &str,
    name_template: Option<&str>,
    arch_entries: &[ArchEntry],
) -> Result<String> {
    let Some(tmpl) = name_template else {
        // No template → checksum stage writes `<asset>.<algorithm>`.
        return Ok(algorithm.to_string());
    };

    // Probe the template with sentinel ArtifactName / ArtifactExt values. The
    // checksum stage exposes ArtifactName / ArtifactExt / Algorithm to the
    // split name_template. Distinct sentinels let us detect, after rendering,
    // whether the asset extension leaked into the suffix portion.
    const SENTINEL: &str = "\u{1}ANODIZER_ASSET\u{1}";
    const SENTINEL_EXT: &str = "\u{1}ANODIZER_EXT\u{1}";
    let mut vars = ctx.template_vars().clone();
    vars.set("ArtifactName", SENTINEL);
    vars.set("ArtifactExt", SENTINEL_EXT);
    vars.set("Algorithm", algorithm);
    let rendered = anodizer_core::template::render(tmpl, &vars).with_context(|| {
        format!("scoop: render split checksum name_template for autoupdate ('{crate_name}')")
    })?;

    // For `$url.<suffix>` to resolve, the sidecar must be exactly
    // `<asset>.<suffix>` — i.e. the rendered name starts with `<sentinel>.`.
    if let Some(suffix) = rendered.strip_prefix(&format!("{SENTINEL}."))
        && !suffix.is_empty()
        && !suffix.contains(SENTINEL)
    {
        // The suffix references the asset extension. A static `$url.<suffix>`
        // only works when every asset shares that extension; otherwise the
        // non-matching asset's sidecar URL 404s.
        if suffix.contains(SENTINEL_EXT) && !windows_assets_uniformly_zip(arch_entries) {
            anyhow::bail!(
                "scoop: cannot build autoupdate.hash for crate '{}': the split checksum \
                 `name_template` ({:?}) embeds the asset extension (`{{{{ ArtifactExt }}}}`) \
                 in the sidecar suffix, but this package's windows assets are not all `.zip` \
                 (scoop also accepts `.tar.gz`/`.tgz`). Scoop's `$url.<suffix>` is a single \
                 static string, so a per-asset-varying extension would 404 for the \
                 non-matching asset. Drop `{{{{ ArtifactExt }}}}` from the suffix (use a \
                 fixed extension like `{{{{ ArtifactName }}}}.sha256`), ship a single \
                 windows archive format, or switch to combined-checksums mode \
                 (`checksum.split: false`).",
                crate_name,
                tmpl,
            );
        }
        // Re-render with the real `.zip` extension so the emitted suffix is
        // concrete (the SENTINEL_EXT token must never reach the manifest).
        let concrete = suffix.replace(SENTINEL_EXT, "zip");
        return Ok(concrete);
    }

    anyhow::bail!(
        "scoop: cannot build autoupdate.hash for crate '{}': the split checksum \
         `name_template` ({:?}) does not produce a per-asset sidecar named \
         `<asset>.<suffix>`, so scoop's `$url.<suffix>` cannot locate it (it \
         would 404 on every auto-bump). Use the default split naming (omit \
         `name_template`, which writes `<asset>.{}`), set a `name_template` of \
         the form `{{{{ ArtifactName }}}}.<ext>`, or switch to combined-checksums \
         mode (`checksum.split: false`).",
        crate_name,
        tmpl,
        algorithm,
    )
}

/// True when every windows asset URL in `arch_entries` ends in `.zip` — the
/// precondition for baking a `{{ ArtifactExt }}`-derived suffix into scoop's
/// single static `$url.<suffix>`. An empty set is vacuously uniform.
fn windows_assets_uniformly_zip(arch_entries: &[ArchEntry]) -> bool {
    arch_entries
        .iter()
        .all(|e| asset_url_extension(&e.url) == Some("zip"))
}

/// The scoop-relevant archive extension of an asset URL: `zip`, `tar.gz`, or
/// `tgz` (the [`SCOOP_PREFERRED_FORMATS`] set), longest-match first so
/// `.tar.gz` is not mis-read as `gz`. `None` when no known archive extension
/// matches.
fn asset_url_extension(url: &str) -> Option<&'static str> {
    let filename = url.rsplit('/').next().unwrap_or(url);
    SCOOP_PREFERRED_FORMATS
        .iter()
        .copied()
        .find(|ext| filename.ends_with(&format!(".{ext}")))
}

/// Generate a Scoop JSON manifest string with extended options.
///
/// Accepts multiple architecture entries. Each entry maps to a key in
/// the `architecture` block: `64bit`, `32bit`, or `arm64`.
pub(crate) fn generate_manifest_with_opts(
    name: &str,
    version: &str,
    arch_entries: &[ArchEntry],
    description: &str,
    license: &str,
    opts: &ManifestOptions<'_>,
) -> Result<String> {
    // Homepage: explicit > GitHub owner/repo > bare name fallback.
    let default_homepage = opts
        .github_slug
        .as_deref()
        .map(|slug| format!("https://github.com/{}", slug))
        .unwrap_or_else(|| format!("https://github.com/{}", name));
    let homepage = opts.homepage.unwrap_or(&default_homepage);

    // Scoop bin entry: use explicit binary names when provided, otherwise
    // derive from the manifest name. Append `.exe` only if not already present.
    let ensure_exe = |b: &str| -> String {
        if b.ends_with(".exe") {
            b.to_string()
        } else {
            format!("{}.exe", b)
        }
    };

    // Scoop `bin` is a flat list of executable names (e.g. `rg.exe`). When the
    // archive wraps its contents in a top-level directory, that directory is
    // expressed once via per-arch `extract_dir` — NOT baked into each bin
    // path. Baking it in breaks `scoop which`/shortcut resolution, which
    // expect a flat extract; the idiomatic ripgrep/fd manifests set
    // `extract_dir` and keep `bin` flat.
    //
    // `bin` is always an array, even for a single binary: validators that pin
    // the schema to `array of strings` reject the singleton-string form.
    let flat_bins: Vec<String> = match opts.bin {
        Some(bins) if !bins.is_empty() => bins.iter().map(|b| ensure_exe(b)).collect(),
        _ => vec![ensure_exe(name)],
    };
    let bin_value = serde_json::json!(flat_bins);

    // Build the architecture block from entries. `extract_dir` is set only when
    // the archive actually wraps in a directory; flat archives must NOT carry
    // an `extract_dir` (scoop would look for a non-existent subdir).
    let mut arch_obj = serde_json::Map::new();
    let mut autoupdate_arch_obj = serde_json::Map::new();
    for entry in arch_entries {
        let wrap = entry.wrap_in_directory.as_deref().filter(|d| !d.is_empty());

        let mut arch_block = serde_json::Map::new();
        arch_block.insert("url".to_string(), serde_json::json!(entry.url));
        arch_block.insert("hash".to_string(), serde_json::json!(entry.hash));
        arch_block.insert("bin".to_string(), bin_value.clone());
        if let Some(dir) = wrap {
            arch_block.insert("extract_dir".to_string(), serde_json::json!(dir));
        }
        arch_obj.insert(
            entry.scoop_arch.clone(),
            serde_json::Value::Object(arch_block),
        );

        // autoupdate per-arch: substitute the concrete version with scoop's
        // `$version` placeholder in both the url and the extract_dir so the
        // bucket auto-bumps on the next release.
        if opts.autoupdate_hash.is_some() {
            let mut au_block = serde_json::Map::new();
            au_block.insert(
                "url".to_string(),
                serde_json::json!(substitute_version(&entry.url, version)),
            );
            if let Some(dir) = wrap {
                au_block.insert(
                    "extract_dir".to_string(),
                    serde_json::json!(substitute_version(dir, version)),
                );
            }
            autoupdate_arch_obj.insert(
                entry.scoop_arch.clone(),
                serde_json::Value::Object(au_block),
            );
        }
    }

    let mut manifest = serde_json::json!({
        "version": version,
        "description": description,
        "homepage": homepage,
        "license": license,
        "architecture": arch_obj
    });

    // Add optional array fields when present. The manifest above is constructed
    // from a `serde_json::json!({...})` object literal; `as_object_mut()` cannot
    // return None unless that literal is changed to a non-object form.
    let obj = manifest
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("scoop: manifest root is not a JSON object"))?;

    // checkver + autoupdate: ScoopInstaller/Main requires both for
    // automated-update PRs. They are emitted together — a checkver without an
    // autoupdate block is a dead half-manifest.
    if let Some(hash_mode) = opts.autoupdate_hash.as_ref() {
        if let Some(checkver) = opts.checkver.as_deref() {
            obj.insert("checkver".to_string(), serde_json::json!(checkver));
        }
        let hash_value = match hash_mode {
            AutoupdateHash::UrlSidecar { suffix } => {
                serde_json::json!({ "url": format!("$url.{suffix}") })
            }
            AutoupdateHash::ChecksumsRegex { url_template } => serde_json::json!({
                "url": url_template,
                // scoop substitutes $basename with the per-arch asset filename;
                // match `<sha256>␠␠<asset>` as emitted by the checksum stage.
                "regex": "$sha256\\s+$basename"
            }),
        };
        let autoupdate = serde_json::json!({
            "architecture": autoupdate_arch_obj,
            "hash": hash_value,
        });
        obj.insert("autoupdate".to_string(), autoupdate);
    }

    if let Some(persist) = opts.persist {
        obj.insert("persist".to_string(), serde_json::json!(persist));
    }
    if let Some(depends) = opts.depends {
        obj.insert("depends".to_string(), serde_json::json!(depends));
    }
    if let Some(pre_install) = opts.pre_install {
        obj.insert("pre_install".to_string(), serde_json::json!(pre_install));
    }
    if let Some(post_install) = opts.post_install {
        obj.insert("post_install".to_string(), serde_json::json!(post_install));
    }
    if let Some(shortcuts) = opts.shortcuts {
        obj.insert("shortcuts".to_string(), serde_json::json!(shortcuts));
    }

    serde_json::to_string_pretty(&manifest).context("scoop: serialize manifest")
}

// ---------------------------------------------------------------------------
// Multi-artifact disambiguation
// ---------------------------------------------------------------------------

/// Format preference for scoop buckets: `.zip` (canonical on Windows) first,
/// then `.tar.gz` / `tgz` as a fallback.
pub(crate) const SCOOP_PREFERRED_FORMATS: &[&str] = &["zip", "tar.gz", "tgz"];

/// Disambiguate a list of `(ArchEntry, format)` pairs when the same
/// `scoop_arch` key appears more than once. Delegates to
/// [`crate::util::disambiguate_by_format`].
pub(crate) fn disambiguate_arch_entries(
    entries: Vec<(ArchEntry, String)>,
    ids_was_set: bool,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Vec<ArchEntry>> {
    let deduped = crate::util::disambiguate_by_format(
        entries,
        |(entry, _)| entry.scoop_arch.clone(),
        |(_, fmt)| fmt.as_str(),
        |(entry, _)| entry.url.clone(),
        crate::util::DisambiguateConfig {
            preferred_formats: SCOOP_PREFERRED_FORMATS,
            ids_was_set,
            publisher_label: "scoop",
            crate_name,
            logger: log,
        },
    )?;
    Ok(deduped.into_iter().map(|(entry, _fmt)| entry).collect())
}
