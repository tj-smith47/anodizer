use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result};

use crate::archive_name::{binstall_pkg_fmt, render_archive_asset_name};
use crate::config::{ArchiveConfig, ArchivesConfig, BinstallConfig, BinstallOverride, CrateConfig};
use crate::context::Context;
use crate::target::map_target;

/// Sentinel substituted into the rendered tag + asset name in place of the
/// release version, then swapped for cargo-binstall's own `{ version }` token.
/// Picked to be vanishingly unlikely to appear in a real project/version string
/// so the post-render substitution is unambiguous.
const VERSION_SENTINEL: &str = "__ANODIZE_BINSTALL_VERSION__";

/// Generate or update `[package.metadata.binstall]` in a crate's Cargo.toml
/// based on the provided BinstallConfig.  The `pkg_url` field is rendered
/// through the template engine so that variables like `{{ .Version }}` and
/// `{{ .Target }}` are expanded.
///
/// # Auto-derivation
///
/// When `binstall.enabled` is set and the user supplied **neither** a top-level
/// `pkg_url` **nor** any `overrides`, anodize auto-derives a per-target
/// `overrides.<rust-triple>` for every configured build target. Each derived
/// override's `pkg_url` is the full GitHub release download URL for that
/// target's archive — its asset name rendered through the *same*
/// `archive.name_template` the archive stage uses, with the version positions
/// expressed as cargo-binstall's `{ version }` token so the URL resolves for
/// whatever version is being installed. Because the asset name is derived from
/// the single source of truth ([`crate::archive_name`]), a derived URL
/// can never drift from the asset the release actually uploads (the "binstall
/// 404" class is eliminated by construction).
///
/// A user-supplied `pkg_url` or any `overrides` entry suppresses
/// auto-derivation entirely — manual values always win.
///
/// The update is performed in place: anodize re-writes only the keys it owns
/// (`pkg-url`, `bin-dir`, `pkg-fmt`, and the `overrides` sub-table). Any other
/// key a user added by hand — cargo-binstall's `disabled-strategies`, the
/// `[package.metadata.binstall.signing]` sub-table, or features anodize does
/// not yet model — is preserved verbatim. An owned key that is now unset in
/// config is cleared, but unknown keys still survive.
pub fn generate_binstall_metadata(
    crate_cfg: &CrateConfig,
    config: &BinstallConfig,
    default_targets: &[String],
    ctx: &mut Context,
    dry_run: bool,
) -> Result<()> {
    let cargo_toml_path = Path::new(&crate_cfg.path).join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml_path)
        .with_context(|| format!("failed to read {}", cargo_toml_path.display()))?;

    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse {}", cargo_toml_path.display()))?;

    // Render the anodize-owned values up front so a template error aborts
    // before any mutation (and before the dry-run short-circuit reports
    // success on a config that would have failed).
    let rendered_pkg_url =
        match config.pkg_url {
            Some(ref pkg_url) => Some(ctx.render_template(pkg_url).with_context(|| {
                format!("failed to render binstall pkg_url template: {}", pkg_url)
            })?),
            None => None,
        };

    // Precedence: a user-supplied top-level `pkg_url` or any explicit
    // `overrides` entry takes full manual control. Auto-derivation engages only
    // when the user supplied NEITHER — the common case where anodize already
    // knows every fact (owner/repo, tag template, per-target asset names) the
    // metadata needs.
    let user_supplied_overrides = config.overrides.as_ref().is_some_and(|o| !o.is_empty());
    let rendered_overrides = if config.pkg_url.is_none() && !user_supplied_overrides {
        let derived = derive_overrides(crate_cfg, default_targets, ctx)?;
        match derived {
            Some(map) => render_overrides_map(&map, ctx)?,
            None => None,
        }
    } else {
        render_overrides(config, ctx)?
    };

    let log = ctx.logger("build");
    if dry_run {
        log.status(&format!(
            "(dry-run) would update [package.metadata.binstall] in {}",
            cargo_toml_path.display()
        ));
        return Ok(());
    }

    // Ensure [package.metadata] exists
    let package = doc
        .get_mut("package")
        .and_then(|p| p.as_table_mut())
        .with_context(|| format!("no [package] table in {}", cargo_toml_path.display()))?;

    if !package.contains_key("metadata") {
        package.insert("metadata", toml_edit::Item::Table(toml_edit::Table::new()));
    }

    let metadata = package
        .get_mut("metadata")
        .and_then(|m| m.as_table_mut())
        .with_context(|| {
            format!(
                "[package].metadata is not a table in {}",
                cargo_toml_path.display()
            )
        })?;

    normalize_binstall_to_table(metadata).with_context(|| {
        format!(
            "[package.metadata.binstall] is neither a table nor an inline table in {}",
            cargo_toml_path.display()
        )
    })?;

    // Merge in place: mutate the existing table so unknown keys
    // (disabled-strategies, signing, future unknowns) survive.
    let binstall = metadata
        .get_mut("binstall")
        .and_then(|b| b.as_table_mut())
        .with_context(|| {
            format!(
                "[package.metadata.binstall] is not a table in {}",
                cargo_toml_path.display()
            )
        })?;

    set_or_remove_str(binstall, "pkg-url", rendered_pkg_url.as_deref());
    set_or_remove_str(binstall, "bin-dir", config.bin_dir.as_deref());
    set_or_remove_str(binstall, "pkg-fmt", config.pkg_fmt.as_deref());

    match rendered_overrides {
        Some(overrides_table) => {
            binstall.insert("overrides", toml_edit::Item::Table(overrides_table));
        }
        None => {
            binstall.remove("overrides");
        }
    }

    std::fs::write(&cargo_toml_path, doc.to_string())
        .with_context(|| format!("failed to write {}", cargo_toml_path.display()))?;

    log.status(&format!(
        "updated [package.metadata.binstall] in {}",
        cargo_toml_path.display()
    ));

    Ok(())
}

/// Ensure `metadata.binstall` is a header table so the in-place merge can
/// mutate it. Three shapes are handled:
///
/// - **missing** — insert an empty header table.
/// - **header table** — already correct; left untouched.
/// - **inline table** (`binstall = { pkg-url = "…" }`) — converted to a header
///   table, preserving every key/value (including user-authored ones anodize
///   does not model) so an inline-metadata user isn't hard-blocked.
///
/// Returns an error only when `binstall` is present but is neither a table nor
/// an inline table (e.g. a scalar/array), which is a malformed manifest.
fn normalize_binstall_to_table(metadata: &mut toml_edit::Table) -> Result<()> {
    match metadata.get("binstall") {
        None => {
            metadata.insert("binstall", toml_edit::Item::Table(toml_edit::Table::new()));
            Ok(())
        }
        Some(item) if item.is_table() => Ok(()),
        Some(item) => {
            let inline = item.as_inline_table().with_context(|| {
                "[package.metadata.binstall] is neither a table nor an inline table".to_string()
            })?;
            // Rebuild as a header table, carrying every existing key/value
            // (anodize-owned and unknown alike) so nothing is dropped.
            let mut table = toml_edit::Table::new();
            for (k, v) in inline.iter() {
                table.insert(k, toml_edit::Item::Value(v.clone()));
            }
            metadata.insert("binstall", toml_edit::Item::Table(table));
            Ok(())
        }
    }
}

/// Set `key` to `value` when present, or remove it when `None`. Removing a
/// now-unset anodize-owned key keeps the merge faithful to config while
/// leaving sibling unknown keys intact.
fn set_or_remove_str(table: &mut toml_edit::Table, key: &str, value: Option<&str>) {
    match value {
        Some(v) => {
            table.insert(key, toml_edit::value(v));
        }
        None => {
            table.remove(key);
        }
    }
}

/// Render the per-target `overrides` sub-table from config, or `None` when no
/// overrides are configured. Override `pkg_url` templates are rendered through
/// the context so anodize tokens expand while cargo-binstall's own `{ ... }`
/// tokens survive intact.
fn render_overrides(config: &BinstallConfig, ctx: &Context) -> Result<Option<toml_edit::Table>> {
    let Some(ref overrides) = config.overrides else {
        return Ok(None);
    };
    if overrides.is_empty() {
        return Ok(None);
    }
    render_overrides_map(overrides, ctx)
}

/// Render a `<triple> -> BinstallOverride` map into a TOML `overrides` table.
/// Shared by the user-supplied path ([`render_overrides`]) and the
/// auto-derived path so both emit identical `[…overrides.<triple>]` headers.
/// `pkg_url` values are rendered through the context so any anodize tokens
/// expand while cargo-binstall's own `{ ... }` tokens survive intact.
fn render_overrides_map(
    overrides: &BTreeMap<String, BinstallOverride>,
    ctx: &Context,
) -> Result<Option<toml_edit::Table>> {
    if overrides.is_empty() {
        return Ok(None);
    }
    let mut overrides_table = toml_edit::Table::new();
    // Render as proper [package.metadata.binstall.overrides.<triple>]
    // headers rather than an inline dotted key.
    overrides_table.set_implicit(true);
    // BTreeMap iteration is sorted, so emission order is deterministic.
    for (triple, ovr) in overrides {
        let mut entry = toml_edit::Table::new();
        if let Some(ref pkg_url) = ovr.pkg_url {
            let rendered = ctx.render_template(pkg_url).with_context(|| {
                format!("failed to render binstall overrides.{triple} pkg_url template: {pkg_url}")
            })?;
            entry.insert("pkg-url", toml_edit::value(rendered));
        }
        if let Some(ref pkg_fmt) = ovr.pkg_fmt {
            entry.insert("pkg-fmt", toml_edit::value(pkg_fmt.as_str()));
        }
        if let Some(ref bin_dir) = ovr.bin_dir {
            entry.insert("bin-dir", toml_edit::value(bin_dir.as_str()));
        }
        overrides_table.insert(triple, toml_edit::Item::Table(entry));
    }
    Ok(Some(overrides_table))
}

// ---------------------------------------------------------------------------
// Auto-derivation
// ---------------------------------------------------------------------------

/// Auto-derive a per-target `overrides` map for `crate_cfg` when binstall is
/// enabled but the user supplied no `pkg_url`/`overrides`.
///
/// For every configured build target, the override's `pkg_url` is the full
/// GitHub release download URL for that target's archive, with the version
/// positions expressed as cargo-binstall's `{ version }` token. The asset name
/// is rendered through the *same* `archive.name_template` the archive stage
/// uses (via [`crate::archive_name`]), so the URL is byte-identical to
/// the asset the release uploads — no drift, no 404.
///
/// Returns `None` (no derivation) when the crate has no release repo, no
/// binstallable archive entry, or no resolvable targets; the surrounding
/// `binstall.enabled` block then emits whatever the user explicitly set
/// (possibly nothing), preserving the manual escape hatch.
fn derive_overrides(
    crate_cfg: &CrateConfig,
    default_targets: &[String],
    ctx: &mut Context,
) -> Result<Option<BTreeMap<String, BinstallOverride>>> {
    let Some((owner, repo, download_base)) = release_repo(crate_cfg, ctx) else {
        return Ok(None);
    };
    let Some(archive) = binstallable_archive(crate_cfg) else {
        return Ok(None);
    };

    let targets = derive_target_list(crate_cfg, default_targets);
    if targets.is_empty() {
        return Ok(None);
    }

    // The tag the release uploads under, with the version expressed as the
    // cargo-binstall `{ version }` token. Rendered once (target-independent) by
    // stamping the sentinel as the version, then swapping it for `{ version }`.
    let tag_with_token = render_tag_with_version_token(crate_cfg, ctx)?;

    // The name template the archive stage will use for this crate (user's
    // `name_template:` wins; otherwise the canonical default — multi-crate when
    // the workspace has more than one crate, matching the archive stage's own
    // single-vs-multi default selection).
    let name_template = archive
        .name_template
        .clone()
        .unwrap_or_else(|| default_archive_name_template(ctx));

    let global_default_format = global_default_archive_format(ctx);

    let mut map: BTreeMap<String, BinstallOverride> = BTreeMap::new();
    for target in &targets {
        let format = archive_format_for_target(&archive, target, &global_default_format);
        let Some(pkg_fmt) = binstall_pkg_fmt(&format) else {
            // A format cargo-binstall cannot binstall (binary / none): no usable
            // override for this target. Skip it rather than emit an unresolvable
            // pkg_fmt.
            continue;
        };

        // Render the asset name with the sentinel version, then swap the
        // sentinel for cargo-binstall's `{ version }` token so the URL resolves
        // for whatever version is being installed.
        let prior = stamp_sentinel_version(ctx);
        let asset = render_archive_asset_name(ctx, &name_template, target, &format);
        restore_version(ctx, prior);
        let asset = asset?;
        let asset_with_token = asset.replace(VERSION_SENTINEL, "{ version }");

        let pkg_url = format!(
            "{download_base}/{owner}/{repo}/releases/download/{tag_with_token}/{asset_with_token}"
        );

        map.insert(
            target.clone(),
            BinstallOverride {
                pkg_url: Some(pkg_url),
                pkg_fmt: Some(pkg_fmt.to_string()),
                bin_dir: None,
            },
        );
    }

    if map.is_empty() {
        return Ok(None);
    }
    Ok(Some(map))
}

/// Resolve the release repo `(owner, repo, download_base)` for `crate_cfg`,
/// rendering any templates in the owner/name. Returns `None` when no GitHub /
/// GitLab / Gitea release repo is configured (auto-derivation cannot build a
/// download URL without one).
fn release_repo(crate_cfg: &CrateConfig, ctx: &Context) -> Option<(String, String, String)> {
    let release = crate_cfg.release.as_ref()?;
    // GitHub is the default download host; GitLab/Gitea use the same
    // `<base>/<owner>/<repo>/releases/download/<tag>/<asset>` path shape, only
    // the host differs. anodize's own config uses GitHub.
    let (repo_cfg, base) = if let Some(gh) = release.github.as_ref() {
        (gh, "https://github.com")
    } else if let Some(gl) = release.gitlab.as_ref() {
        (gl, "https://gitlab.com")
    } else if let Some(gt) = release.gitea.as_ref() {
        (gt, "https://gitea.com")
    } else {
        return None;
    };
    let owner = ctx.render_template(&repo_cfg.owner).ok()?;
    let repo = ctx.render_template(&repo_cfg.name).ok()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo, base.to_string()))
}

/// Pick the archive entry cargo-binstall should install from: the first entry
/// whose default format is binstallable (tar.gz / zip / …),
/// the primary archive is the one consumers fetch; auxiliary entries (e.g. a
/// `tar.xz`/`tar.zst` `-extra` entry) are skipped.
fn binstallable_archive(crate_cfg: &CrateConfig) -> Option<ArchiveConfig> {
    let ArchivesConfig::Configs(configs) = &crate_cfg.archives else {
        return None;
    };
    configs
        .iter()
        .find(|a| {
            a.formats
                .as_ref()
                .and_then(|f| f.first())
                .map(|fmt| binstall_pkg_fmt(fmt).is_some())
                .unwrap_or(true)
        })
        .cloned()
        .or_else(|| configs.first().cloned())
}

/// Resolve the full set of target triples binstall metadata must cover for
/// `crate_cfg`: the union of each build's targets (a build's own `targets:`
/// when set, else the global `default_targets`), de-duplicated and sorted.
/// Mirrors the build stage's per-build target resolution so the derived
/// override set equals the released asset set.
fn derive_target_list(crate_cfg: &CrateConfig, default_targets: &[String]) -> Vec<String> {
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    let builds = crate_cfg.builds.as_deref().unwrap_or(&[]);
    if builds.is_empty() {
        for t in default_targets {
            seen.insert(t.clone(), ());
        }
    } else {
        for build in builds {
            let targets: &[String] = match build.targets.as_deref() {
                Some(ts) => ts,
                None => default_targets,
            };
            for t in targets {
                seen.insert(t.clone(), ());
            }
        }
    }
    seen.into_keys().collect()
}

/// Render the crate's tag template with the version expressed as
/// cargo-binstall's `{ version }` token. Stamps the [`VERSION_SENTINEL`] as the
/// version, renders, then swaps the sentinel for `{ version }`.
fn render_tag_with_version_token(crate_cfg: &CrateConfig, ctx: &mut Context) -> Result<String> {
    // Prefer an explicit `release.tag` override; otherwise the crate's
    // `tag_template` (defaulting to `v{{ Version }}` shape when unset).
    let tag_template = crate_cfg
        .release
        .as_ref()
        .and_then(|r| r.tag.clone())
        .filter(|t| !t.is_empty())
        .or_else(|| Some(crate_cfg.tag_template.clone()).filter(|t| !t.is_empty()))
        .unwrap_or_else(|| "v{{ Version }}".to_string());

    let prior = stamp_sentinel_version(ctx);
    let rendered = ctx.render_template(&tag_template);
    restore_version(ctx, prior);
    let rendered = rendered
        .with_context(|| format!("failed to render binstall tag template: {tag_template}"))?;
    Ok(rendered.replace(VERSION_SENTINEL, "{ version }"))
}

/// Stamp the version-related template vars (`Version`, `RawVersion`, `Tag`) to
/// the sentinel and return the prior values for [`restore_version`]. Tag is
/// stamped too so a `name_template` referencing `{{ Tag }}` also picks up the
/// sentinel.
fn stamp_sentinel_version(ctx: &mut Context) -> Vec<(&'static str, Option<String>)> {
    let prior: Vec<(&'static str, Option<String>)> = ["Version", "RawVersion", "Tag"]
        .iter()
        .map(|k| (*k, ctx.template_vars().get(k).cloned()))
        .collect();

    let vars = ctx.template_vars_mut();
    vars.set("Version", VERSION_SENTINEL);
    vars.set("RawVersion", VERSION_SENTINEL);
    // A literal `v<sentinel>` keeps `{{ Tag }}`-based name templates aligned
    // with the `v{ version }` tag the download URL targets.
    vars.set("Tag", &format!("v{VERSION_SENTINEL}"));
    prior
}

/// Restore the version vars captured by [`stamp_sentinel_version`].
fn restore_version(ctx: &mut Context, prior: Vec<(&'static str, Option<String>)>) {
    let vars = ctx.template_vars_mut();
    for (key, value) in prior {
        match value {
            Some(v) => vars.set(key, &v),
            None => {
                vars.unset(key);
            }
        }
    }
}

/// The default `archive.name_template` the archive stage uses for this crate:
/// the multi-crate default when the workspace has more than one crate, else the
/// single-crate default. Matches the archive stage's own default selection.
fn default_archive_name_template(ctx: &Context) -> String {
    if ctx.config.crates.len() > 1 {
        crate::archive_name::DEFAULT_NAME_TEMPLATE_MULTI_CRATE.to_string()
    } else {
        crate::archive_name::DEFAULT_NAME_TEMPLATE.to_string()
    }
}

/// The project-wide default archive format (`defaults.archives.formats[0]`,
/// falling back to `tar.gz`). Used when an archive entry sets no `formats:`.
fn global_default_archive_format(ctx: &Context) -> String {
    ctx.config
        .defaults
        .as_ref()
        .and_then(|d| d.archives.as_ref())
        .and_then(|a| a.formats.as_ref())
        .and_then(|f| f.first())
        .cloned()
        .unwrap_or_else(|| "tar.gz".to_string())
}

/// The archive format an entry produces for `target`: the first matching
/// `format_overrides[]` entry's format (OS-prefix match, mirroring the archive
/// stage), else the entry's own first `formats[]`, else `global_default`.
fn archive_format_for_target(
    archive: &ArchiveConfig,
    target: &str,
    global_default: &str,
) -> String {
    let (os, _arch) = map_target(target);
    if let Some(overrides) = archive.format_overrides.as_ref() {
        for ov in overrides {
            if !ov.os.is_empty()
                && os.starts_with(&ov.os)
                && let Some(fmts) = ov.formats.as_ref()
                && let Some(first) = fmts.first()
            {
                return first.clone();
            }
        }
    }
    archive
        .formats
        .as_ref()
        .and_then(|f| f.first())
        .cloned()
        .unwrap_or_else(|| global_default.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{
        ArchiveConfig, ArchivesConfig, BinstallConfig, BinstallOverride, Config, FormatOverride,
        GitHubConfig, ReleaseConfig,
    };
    use crate::context::{Context, ContextOptions};

    fn make_ctx() -> Context {
        let config = Config {
            project_name: "myapp".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");
        ctx
    }

    /// Drive [`generate_binstall_metadata`] against a temp-dir crate with the
    /// given binstall config. The synthesized `CrateConfig` carries no archives
    /// / release / builds, so auto-derivation is inert unless the test supplies
    /// a richer crate via [`gen_with_crate`] — matching the bare
    /// `(path, cfg, ctx)` call shape the surrounding tests use.
    fn gen_meta(path: &str, cfg: &BinstallConfig, ctx: &mut Context, dry_run: bool) -> Result<()> {
        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: path.to_string(),
            ..Default::default()
        };
        generate_binstall_metadata(&crate_cfg, cfg, &[], ctx, dry_run)
    }

    /// Like [`gen`] but with a caller-supplied `CrateConfig` (path is forced to
    /// `path`) and an explicit `default_targets` list, for auto-derivation tests.
    fn gen_with_crate(
        path: &str,
        mut crate_cfg: CrateConfig,
        cfg: &BinstallConfig,
        default_targets: &[String],
        ctx: &mut Context,
    ) -> Result<()> {
        crate_cfg.path = path.to_string();
        generate_binstall_metadata(&crate_cfg, cfg, default_targets, ctx, false)
    }

    #[test]
    fn test_generate_binstall_metadata_inserts_section() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "myapp"
version = "1.0.0"
edition = "2024"
"#,
        )
        .unwrap();

        let binstall_cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: Some(
                "https://github.com/myorg/myapp/releases/download/v{{ .Version }}/myapp-{{ .Version }}-{ target }.tar.gz"
                    .to_string(),
            ),
            bin_dir: Some("{ bin }{ binary-ext }".to_string()),
            pkg_fmt: Some("tgz".to_string()),
            overrides: None,
        };

        let mut ctx = make_ctx();
        gen_meta(tmp.path().to_str().unwrap(), &binstall_cfg, &mut ctx, false).unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();

        let binstall = &doc["package"]["metadata"]["binstall"];
        assert!(
            binstall.as_table().is_some(),
            "binstall section should exist as a table"
        );
        assert_eq!(binstall["pkg-fmt"].as_str().unwrap(), "tgz");
        assert_eq!(
            binstall["bin-dir"].as_str().unwrap(),
            "{ bin }{ binary-ext }"
        );
        // The pkg-url should have the template variable rendered
        let pkg_url = binstall["pkg-url"].as_str().unwrap();
        assert!(
            pkg_url.contains("1.0.0"),
            "pkg-url should have Version rendered, got: {pkg_url}"
        );
    }

    #[test]
    fn test_generate_binstall_metadata_dry_run() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        let original = r#"[package]
name = "myapp"
version = "1.0.0"
edition = "2024"
"#;
        std::fs::write(&cargo_toml, original).unwrap();

        let binstall_cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: Some("https://example.com".to_string()),
            bin_dir: None,
            pkg_fmt: None,
            overrides: None,
        };

        let mut ctx = make_ctx();
        gen_meta(tmp.path().to_str().unwrap(), &binstall_cfg, &mut ctx, true).unwrap();

        // File should be unchanged in dry-run mode
        let content = std::fs::read_to_string(&cargo_toml).unwrap();
        assert_eq!(content, original);
    }

    #[test]
    fn test_generate_binstall_metadata_missing_cargo_toml_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let binstall_cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: None,
            bin_dir: None,
            pkg_fmt: None,
            overrides: None,
        };

        let mut ctx = make_ctx();
        let result = gen_meta(tmp.path().to_str().unwrap(), &binstall_cfg, &mut ctx, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_generate_binstall_metadata_emits_per_target_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "cfgd"
version = "1.0.0"
edition = "2024"
"#,
        )
        .unwrap();

        let mut overrides = BTreeMap::new();
        overrides.insert(
            "x86_64-unknown-linux-gnu".to_string(),
            BinstallOverride {
                pkg_url: Some(
                    "https://github.com/myorg/cfgd/releases/download/v{{ .Version }}/cfgd-{{ .Version }}-linux-amd64.tar.gz"
                        .to_string(),
                ),
                pkg_fmt: Some("tgz".to_string()),
                bin_dir: Some("{ bin }{ binary-ext }".to_string()),
            },
        );
        overrides.insert(
            "aarch64-apple-darwin".to_string(),
            BinstallOverride {
                pkg_url: Some(
                    "https://github.com/myorg/cfgd/releases/download/v{{ .Version }}/cfgd-{ version }-darwin-arm64.tar.gz"
                        .to_string(),
                ),
                pkg_fmt: Some("tgz".to_string()),
                bin_dir: None,
            },
        );

        let binstall_cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: None,
            bin_dir: None,
            pkg_fmt: None,
            overrides: Some(overrides),
        };

        let mut ctx = make_ctx();
        gen_meta(tmp.path().to_str().unwrap(), &binstall_cfg, &mut ctx, false).unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();

        let overrides_item = &doc["package"]["metadata"]["binstall"]["overrides"];
        assert!(
            overrides_item.as_table().is_some(),
            "binstall.overrides should exist as a table"
        );

        // linux-amd64 (go-arch) entry: Version rendered, asset name intact.
        let linux = &overrides_item["x86_64-unknown-linux-gnu"];
        assert!(
            linux.as_table().is_some(),
            "override sub-table should be a real table"
        );
        let linux_url = linux["pkg-url"].as_str().unwrap();
        assert!(
            linux_url.contains("cfgd-1.0.0-linux-amd64.tar.gz"),
            "linux pkg-url should be Version-rendered with go-arch asset name, got: {linux_url}"
        );
        assert!(
            !linux_url.contains("{{ .Version }}"),
            "anodize token should be rendered, got: {linux_url}"
        );
        assert_eq!(linux["pkg-fmt"].as_str().unwrap(), "tgz");
        assert_eq!(linux["bin-dir"].as_str().unwrap(), "{ bin }{ binary-ext }");

        // darwin-arm64 (go-arch) entry.
        let darwin = &overrides_item["aarch64-apple-darwin"];
        assert!(darwin.as_table().is_some());
        let darwin_url = darwin["pkg-url"].as_str().unwrap();
        // The leading v{{ .Version }} is an anodize token (rendered) while
        // `{ version }` is cargo-binstall's own token and must survive intact.
        assert!(
            darwin_url.contains("/v1.0.0/cfgd-{ version }-darwin-arm64.tar.gz"),
            "darwin pkg-url should render the anodize token but leave cargo-binstall's `{{ version }}` intact, got: {darwin_url}"
        );

        // Triple keys contain `-` and must render as proper headers.
        assert!(
            updated.contains("[package.metadata.binstall.overrides.x86_64-unknown-linux-gnu]"),
            "override should render as a [...] header, got:\n{updated}"
        );
    }

    #[test]
    fn test_generate_binstall_metadata_preserves_user_authored_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        // Seed a Cargo.toml whose binstall table already carries keys anodize
        // does NOT model: cargo-binstall's `disabled-strategies` and the
        // `[package.metadata.binstall.signing]` sub-table. The in-place merge
        // must leave both untouched while (re)writing pkg-url / overrides.
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "myapp"
version = "1.0.0"
edition = "2024"

[package.metadata.binstall]
disabled-strategies = ["quick-install", "compile"]
pkg-url = "https://old.example.com/stale"

[package.metadata.binstall.signing]
algorithm = "minisign"
pubkey = "RWQABCDEF1234567890"
"#,
        )
        .unwrap();

        let mut overrides = BTreeMap::new();
        overrides.insert(
            "x86_64-unknown-linux-gnu".to_string(),
            BinstallOverride {
                pkg_url: Some(
                    "https://github.com/myorg/myapp/releases/download/v{{ .Version }}/myapp-linux.tar.gz"
                        .to_string(),
                ),
                pkg_fmt: Some("tgz".to_string()),
                bin_dir: None,
            },
        );

        let binstall_cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: Some(
                "https://github.com/myorg/myapp/releases/download/v{{ .Version }}/myapp-{ target }.tar.gz"
                    .to_string(),
            ),
            bin_dir: None,
            pkg_fmt: None,
            overrides: Some(overrides),
        };

        let mut ctx = make_ctx();
        gen_meta(tmp.path().to_str().unwrap(), &binstall_cfg, &mut ctx, false).unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();
        let binstall = &doc["package"]["metadata"]["binstall"];

        // Unknown keys survive verbatim.
        let strategies = binstall["disabled-strategies"].as_array().unwrap();
        let strategy_vals: Vec<&str> = strategies.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            strategy_vals,
            vec!["quick-install", "compile"],
            "disabled-strategies must survive the merge verbatim"
        );

        let signing = &binstall["signing"];
        assert!(
            signing.as_table().is_some(),
            "signing sub-table must survive the merge"
        );
        assert_eq!(signing["algorithm"].as_str().unwrap(), "minisign");
        assert_eq!(signing["pubkey"].as_str().unwrap(), "RWQABCDEF1234567890");

        // anodize-owned keys are (re)written: pkg-url rendered to the new value.
        let pkg_url = binstall["pkg-url"].as_str().unwrap();
        assert!(
            pkg_url.contains("/v1.0.0/myapp-{ target }.tar.gz"),
            "pkg-url should be rewritten with the rendered Version, got: {pkg_url}"
        );
        assert!(
            !pkg_url.contains("old.example.com"),
            "stale anodize-owned pkg-url should be replaced, got: {pkg_url}"
        );

        // overrides is anodize-owned and freshly written.
        let linux = &binstall["overrides"]["x86_64-unknown-linux-gnu"];
        assert_eq!(linux["pkg-fmt"].as_str().unwrap(), "tgz");
        assert!(
            linux["pkg-url"]
                .as_str()
                .unwrap()
                .contains("/v1.0.0/myapp-linux.tar.gz")
        );
    }

    #[test]
    fn test_generate_binstall_metadata_clears_unset_owned_key_keeps_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        // pkg-url present plus an unknown sibling key. Config omits pkg_url, so
        // the owned key must be cleared while the unknown key survives.
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "myapp"
version = "1.0.0"
edition = "2024"

[package.metadata.binstall]
disabled-strategies = ["compile"]
pkg-url = "https://old.example.com/stale"
"#,
        )
        .unwrap();

        let binstall_cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: None,
            bin_dir: None,
            pkg_fmt: None,
            overrides: None,
        };

        let mut ctx = make_ctx();
        gen_meta(tmp.path().to_str().unwrap(), &binstall_cfg, &mut ctx, false).unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();
        let binstall = &doc["package"]["metadata"]["binstall"];

        assert!(
            binstall.get("pkg-url").is_none(),
            "unset owned key should be cleared, got:\n{updated}"
        );
        let strategies = binstall["disabled-strategies"].as_array().unwrap();
        assert_eq!(strategies.len(), 1, "unknown key must survive clearing");
        assert_eq!(strategies.get(0).unwrap().as_str().unwrap(), "compile");
    }

    #[test]
    fn test_generate_binstall_metadata_merges_inline_table_preserving_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        // A user with INLINE binstall metadata: `as_table_mut()` would return
        // None on this. The merge must convert it to a header table while
        // preserving the unknown `disabled-strategies` key.
        std::fs::write(
            &cargo_toml,
            r#"[package]
name = "myapp"
version = "1.0.0"
edition = "2024"
metadata.binstall = { pkg-url = "https://old.example.com/stale", disabled-strategies = ["compile"] }
"#,
        )
        .unwrap();

        let binstall_cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: Some(
                "https://github.com/myorg/myapp/releases/download/v{{ .Version }}/myapp-{ target }.tar.gz"
                    .to_string(),
            ),
            bin_dir: None,
            pkg_fmt: None,
            overrides: None,
        };

        let mut ctx = make_ctx();
        gen_meta(tmp.path().to_str().unwrap(), &binstall_cfg, &mut ctx, false).unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();
        let binstall = &doc["package"]["metadata"]["binstall"];

        // pkg-url rewritten with the rendered Version.
        let pkg_url = binstall["pkg-url"].as_str().unwrap();
        assert!(
            pkg_url.contains("/v1.0.0/myapp-{ target }.tar.gz")
                && !pkg_url.contains("old.example.com"),
            "inline pkg-url should be rewritten, got: {pkg_url}"
        );
        // Unknown key carried over from the inline table.
        let strategies = binstall["disabled-strategies"].as_array().unwrap();
        assert_eq!(strategies.len(), 1);
        assert_eq!(strategies.get(0).unwrap().as_str().unwrap(), "compile");
    }

    // -----------------------------------------------------------------------
    // Auto-derivation
    // -----------------------------------------------------------------------

    /// All six anodize triples, the matrix the auto-derivation must cover.
    fn six_targets() -> Vec<String> {
        vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
            "x86_64-apple-darwin".to_string(),
            "aarch64-apple-darwin".to_string(),
            "x86_64-pc-windows-msvc".to_string(),
            "aarch64-pc-windows-msvc".to_string(),
        ]
    }

    /// A crate mirroring anodize's binary crate: an explicit
    /// `name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"`,
    /// `formats: [tar.gz]` with a windows→zip override, and a GitHub release.
    fn anodize_like_crate() -> CrateConfig {
        let archive = ArchiveConfig {
            name_template: Some("{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}".to_string()),
            formats: Some(vec!["tar.gz".to_string()]),
            format_overrides: Some(vec![FormatOverride {
                os: "windows".to_string(),
                formats: Some(vec!["zip".to_string()]),
            }]),
            ..Default::default()
        };
        CrateConfig {
            name: "anodizer".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            archives: ArchivesConfig::Configs(vec![archive]),
            release: Some(ReleaseConfig {
                github: Some(GitHubConfig {
                    owner: "tj-smith47".to_string(),
                    name: "anodizer".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// The asset filename a binstall override resolves to, with the
    /// cargo-binstall `{ version }` token substituted back to the test version.
    fn override_asset(binstall: &toml_edit::Item, triple: &str, version: &str) -> String {
        let url = binstall["overrides"][triple]["pkg-url"].as_str().unwrap();
        let leaf = url.rsplit('/').next().unwrap();
        leaf.replace("{ version }", version)
    }

    #[test]
    fn auto_derive_covers_all_six_triples_matching_archive_names() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            "[package]\nname = \"anodizer\"\nversion = \"1.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();

        // enabled: true, NOTHING else — the whole point.
        let cfg = BinstallConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let mut ctx = make_ctx();
        ctx.template_vars_mut().set("ProjectName", "anodizer");
        let targets = six_targets();
        gen_with_crate(
            tmp.path().to_str().unwrap(),
            anodize_like_crate(),
            &cfg,
            &targets,
            &mut ctx,
        )
        .unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();
        let binstall = &doc["package"]["metadata"]["binstall"];

        // Every triple got an override, and each override's asset name EQUALS
        // what the shared archive name-rendering produces for that target —
        // the byte-equality that kills the 404 class. Render the expectation
        // through the SAME core function the archive stage uses.
        let mut check_ctx = make_ctx();
        check_ctx.template_vars_mut().set("ProjectName", "anodizer");
        let name_tmpl = "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}";
        for triple in &targets {
            let fmt = if triple.contains("windows") {
                "zip"
            } else {
                "tar.gz"
            };
            let expected = crate::archive_name::render_archive_asset_name(
                &mut check_ctx,
                name_tmpl,
                triple,
                fmt,
            )
            .unwrap();
            let actual = override_asset(binstall, triple, "1.0.0");
            assert_eq!(
                actual, expected,
                "override asset for {triple} must equal the archive stage's name"
            );

            // pkg-fmt matches the format.
            let pkg_fmt = binstall["overrides"][triple]["pkg-fmt"].as_str().unwrap();
            assert_eq!(pkg_fmt, if fmt == "zip" { "zip" } else { "tgz" });

            // cargo-binstall's `{ version }` token is present (resolves per
            // install), and the full download URL is well-formed.
            let url = binstall["overrides"][triple]["pkg-url"].as_str().unwrap();
            assert!(
                url.starts_with(
                    "https://github.com/tj-smith47/anodizer/releases/download/v{ version }/"
                ),
                "url should target the GitHub release download path with the binstall version token, got: {url}"
            );
            assert!(
                url.contains("{ version }"),
                "url must carry cargo-binstall's version token, got: {url}"
            );
        }

        // Concrete spot-checks for the two endpoints.
        assert_eq!(
            override_asset(binstall, "x86_64-unknown-linux-gnu", "9.9.9"),
            "anodizer-9.9.9-linux-amd64.tar.gz"
        );
        assert_eq!(
            override_asset(binstall, "aarch64-pc-windows-msvc", "9.9.9"),
            "anodizer-9.9.9-windows-arm64.zip"
        );

        // No stale top-level pkg-url leaks in.
        assert!(
            binstall.get("pkg-url").is_none(),
            "auto-derivation must not write a top-level pkg-url, got:\n{updated}"
        );
    }

    #[test]
    fn auto_derive_matches_real_v091_release_assets() {
        // Real-world guard: anodize's own crate config (owner/repo/name_template/
        // tag_template from `.anodizer.yaml`) must auto-derive overrides whose
        // `pkg-url`s resolve, for a concrete version, to the EXACT asset names
        // the v0.9.1 GitHub release uploaded. A drift here is the "cargo binstall
        // 404 / source-compile" bug this whole path exists to prevent. The
        // expected strings are the literal v0.9.1 release assets, hand-written so
        // a refactor of the rendering can't move both sides in lockstep.
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            "[package]\nname = \"anodizer\"\nversion = \"0.9.1\"\nedition = \"2024\"\n",
        )
        .unwrap();

        let cfg = BinstallConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let mut ctx = make_ctx();
        ctx.template_vars_mut().set("ProjectName", "anodizer");
        gen_with_crate(
            tmp.path().to_str().unwrap(),
            anodize_like_crate(),
            &cfg,
            &six_targets(),
            &mut ctx,
        )
        .unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();
        let binstall = &doc["package"]["metadata"]["binstall"];

        // (triple, real v0.9.1 asset name, real v0.9.1 pkg-fmt)
        let expected: &[(&str, &str, &str)] = &[
            (
                "x86_64-unknown-linux-gnu",
                "anodizer-0.9.1-linux-amd64.tar.gz",
                "tgz",
            ),
            (
                "aarch64-unknown-linux-gnu",
                "anodizer-0.9.1-linux-arm64.tar.gz",
                "tgz",
            ),
            (
                "x86_64-apple-darwin",
                "anodizer-0.9.1-darwin-amd64.tar.gz",
                "tgz",
            ),
            (
                "aarch64-apple-darwin",
                "anodizer-0.9.1-darwin-arm64.tar.gz",
                "tgz",
            ),
            (
                "x86_64-pc-windows-msvc",
                "anodizer-0.9.1-windows-amd64.zip",
                "zip",
            ),
            (
                "aarch64-pc-windows-msvc",
                "anodizer-0.9.1-windows-arm64.zip",
                "zip",
            ),
        ];

        for (triple, asset, fmt) in expected {
            let url = binstall["overrides"][*triple]["pkg-url"].as_str().unwrap();
            // The override carries cargo-binstall's `{ version }` token; resolved
            // for 0.9.1 it must equal the real release download URL byte-for-byte.
            let resolved = url.replace("{ version }", "0.9.1");
            let want =
                format!("https://github.com/tj-smith47/anodizer/releases/download/v0.9.1/{asset}");
            assert_eq!(
                resolved, want,
                "override for {triple} must resolve to the real v0.9.1 asset URL"
            );
            assert_eq!(
                binstall["overrides"][*triple]["pkg-fmt"].as_str().unwrap(),
                *fmt,
                "pkg-fmt for {triple} must match the real v0.9.1 asset format"
            );
        }
    }

    #[test]
    fn user_pkg_url_suppresses_auto_derivation() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            "[package]\nname = \"anodizer\"\nversion = \"1.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();

        let cfg = BinstallConfig {
            enabled: Some(true),
            pkg_url: Some("https://example.com/custom/anodizer-{ target }.tar.gz".to_string()),
            ..Default::default()
        };
        let mut ctx = make_ctx();
        ctx.template_vars_mut().set("ProjectName", "anodizer");
        gen_with_crate(
            tmp.path().to_str().unwrap(),
            anodize_like_crate(),
            &cfg,
            &six_targets(),
            &mut ctx,
        )
        .unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();
        let binstall = &doc["package"]["metadata"]["binstall"];

        // Manual pkg-url wins; NO auto-derived overrides table is emitted.
        assert_eq!(
            binstall["pkg-url"].as_str().unwrap(),
            "https://example.com/custom/anodizer-{ target }.tar.gz"
        );
        assert!(
            binstall.get("overrides").is_none(),
            "a user pkg_url must suppress auto-derived overrides, got:\n{updated}"
        );
    }

    #[test]
    fn user_override_suppresses_auto_derivation() {
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            "[package]\nname = \"anodizer\"\nversion = \"1.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();

        let mut user_overrides = BTreeMap::new();
        user_overrides.insert(
            "x86_64-unknown-linux-gnu".to_string(),
            BinstallOverride {
                pkg_url: Some("https://example.com/manual-linux.tar.gz".to_string()),
                pkg_fmt: Some("tgz".to_string()),
                bin_dir: None,
            },
        );
        let cfg = BinstallConfig {
            enabled: Some(true),
            overrides: Some(user_overrides),
            ..Default::default()
        };
        let mut ctx = make_ctx();
        ctx.template_vars_mut().set("ProjectName", "anodizer");
        gen_with_crate(
            tmp.path().to_str().unwrap(),
            anodize_like_crate(),
            &cfg,
            &six_targets(),
            &mut ctx,
        )
        .unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();
        let overrides = &doc["package"]["metadata"]["binstall"]["overrides"];

        // ONLY the user's single override is emitted — auto-derivation does not
        // add the other five triples.
        assert_eq!(
            overrides["x86_64-unknown-linux-gnu"]["pkg-url"]
                .as_str()
                .unwrap(),
            "https://example.com/manual-linux.tar.gz"
        );
        assert!(
            overrides.get("aarch64-apple-darwin").is_none(),
            "supplying one override must suppress auto-derivation of the rest, got:\n{updated}"
        );
    }

    #[test]
    fn auto_derive_uses_default_template_when_unset() {
        // A crate with binstall enabled but NO explicit archive name_template
        // must derive against the canonical default (single-crate) template.
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            "[package]\nname = \"myapp\"\nversion = \"1.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();

        let mut crate_cfg = anodize_like_crate();
        crate_cfg.name = "myapp".to_string();
        // Archive with formats but no name_template.
        crate_cfg.archives = ArchivesConfig::Configs(vec![ArchiveConfig {
            formats: Some(vec!["tar.gz".to_string()]),
            ..Default::default()
        }]);

        let cfg = BinstallConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let mut ctx = make_ctx();
        gen_with_crate(
            tmp.path().to_str().unwrap(),
            crate_cfg,
            &cfg,
            &["x86_64-unknown-linux-gnu".to_string()],
            &mut ctx,
        )
        .unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();
        let binstall = &doc["package"]["metadata"]["binstall"];
        // Default template → `myapp_1.0.0_linux_amd64.tar.gz`.
        assert_eq!(
            override_asset(binstall, "x86_64-unknown-linux-gnu", "1.0.0"),
            "myapp_1.0.0_linux_amd64.tar.gz"
        );
    }

    #[test]
    fn auto_derive_noops_without_release_repo() {
        // No release repo → no download URL can be built → no overrides.
        let tmp = tempfile::tempdir().unwrap();
        let cargo_toml = tmp.path().join("Cargo.toml");
        std::fs::write(
            &cargo_toml,
            "[package]\nname = \"myapp\"\nversion = \"1.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();

        let mut crate_cfg = anodize_like_crate();
        crate_cfg.name = "myapp".to_string();
        crate_cfg.release = None;

        let cfg = BinstallConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let mut ctx = make_ctx();
        gen_with_crate(
            tmp.path().to_str().unwrap(),
            crate_cfg,
            &cfg,
            &six_targets(),
            &mut ctx,
        )
        .unwrap();

        let updated = std::fs::read_to_string(&cargo_toml).unwrap();
        let doc = updated.parse::<toml_edit::DocumentMut>().unwrap();
        let binstall = &doc["package"]["metadata"]["binstall"];
        assert!(
            binstall.get("overrides").is_none(),
            "no release repo should mean no auto-derived overrides, got:\n{updated}"
        );
    }

    // --- normalize_binstall_to_table --------------------------------------

    #[test]
    fn normalize_inserts_empty_table_when_binstall_absent() {
        let mut metadata = toml_edit::Table::new();
        normalize_binstall_to_table(&mut metadata).unwrap();
        let binstall = metadata.get("binstall").expect("binstall inserted");
        assert!(binstall.is_table(), "must be a header table");
        assert!(
            binstall.as_table().unwrap().is_empty(),
            "freshly-inserted binstall table must be empty"
        );
    }

    #[test]
    fn normalize_leaves_existing_header_table_untouched() {
        let mut metadata = toml_edit::Table::new();
        let mut existing = toml_edit::Table::new();
        existing.insert("pkg-url", toml_edit::value("https://example/x"));
        metadata.insert("binstall", toml_edit::Item::Table(existing));
        normalize_binstall_to_table(&mut metadata).unwrap();
        assert_eq!(
            metadata["binstall"]["pkg-url"].as_str(),
            Some("https://example/x"),
            "an existing header table must be preserved verbatim"
        );
    }

    #[test]
    fn normalize_converts_inline_table_preserving_keys() {
        // `binstall = { pkg-url = "…", custom = "keep" }` → header table with
        // every key carried over (including the user-authored `custom`).
        let doc: toml_edit::DocumentMut = r#"[package.metadata]
binstall = { pkg-url = "https://example/x", custom = "keep" }
"#
        .parse()
        .unwrap();
        let mut metadata = doc["package"]["metadata"].as_table().unwrap().clone();
        normalize_binstall_to_table(&mut metadata).unwrap();
        let binstall = metadata["binstall"].as_table().expect("now a header table");
        assert_eq!(binstall["pkg-url"].as_str(), Some("https://example/x"));
        assert_eq!(
            binstall["custom"].as_str(),
            Some("keep"),
            "user-authored inline keys must survive the conversion"
        );
    }

    #[test]
    fn normalize_errors_on_non_table_binstall() {
        // A scalar `binstall = "oops"` is neither a table nor an inline table.
        let mut metadata = toml_edit::Table::new();
        metadata.insert("binstall", toml_edit::value("oops"));
        assert!(
            normalize_binstall_to_table(&mut metadata).is_err(),
            "a malformed scalar binstall must error, not silently pass"
        );
    }

    // --- set_or_remove_str ------------------------------------------------

    #[test]
    fn set_or_remove_str_sets_and_removes() {
        let mut table = toml_edit::Table::new();
        set_or_remove_str(&mut table, "k", Some("v"));
        assert_eq!(table["k"].as_str(), Some("v"));
        // A `None` value removes a now-unset key while leaving siblings intact.
        table.insert("sibling", toml_edit::value("keep"));
        set_or_remove_str(&mut table, "k", None);
        assert!(table.get("k").is_none(), "None must remove the key");
        assert_eq!(
            table["sibling"].as_str(),
            Some("keep"),
            "removing one key must not disturb siblings"
        );
    }

    // --- render_overrides / render_overrides_map --------------------------

    #[test]
    fn render_overrides_none_when_unset_or_empty() {
        let ctx = make_ctx();
        let cfg = BinstallConfig {
            overrides: None,
            ..Default::default()
        };
        assert!(
            render_overrides(&cfg, &ctx).unwrap().is_none(),
            "no overrides configured → None"
        );
        let cfg_empty = BinstallConfig {
            overrides: Some(BTreeMap::new()),
            ..Default::default()
        };
        assert!(
            render_overrides(&cfg_empty, &ctx).unwrap().is_none(),
            "an empty overrides map → None"
        );
    }

    #[test]
    fn render_overrides_map_empty_is_none() {
        let ctx = make_ctx();
        let empty: BTreeMap<String, BinstallOverride> = BTreeMap::new();
        assert!(
            render_overrides_map(&empty, &ctx).unwrap().is_none(),
            "an empty map renders to None, never an empty table"
        );
    }
}
