use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::SrpmConfig;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

// ---------------------------------------------------------------------------
// SrpmStage
// ---------------------------------------------------------------------------

pub struct SrpmStage;

impl Stage for SrpmStage {
    fn name(&self) -> &str {
        "srpm"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("srpm");
        let srpm_cfg = match ctx.config.srpms.clone() {
            Some(cfg) if cfg.enabled.unwrap_or(false) => cfg,
            _ => return Ok(()),
        };

        // Check disable
        if let Some(ref d) = srpm_cfg.skip {
            let off = d
                .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                .with_context(|| "srpm: render skip template")?;
            if off {
                log.verbose("SRPM config skipped");
                return Ok(());
            }
        }

        // when global skip_sign is active, clear signature config
        let skip_sign = ctx.should_skip("sign");

        let dist = ctx.config.dist.clone();
        let dry_run = ctx.options.dry_run;
        let project_name = ctx.config.project_name.clone();
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());

        // Find source archives — clone to release borrow on ctx
        let source_archives: Vec<Artifact> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::SourceArchive)
            .cloned()
            .collect();

        if source_archives.is_empty() {
            if ctx.options.snapshot || dry_run {
                log.verbose("skipping SRPM: no source archives found (snapshot/dry-run mode)");
                return Ok(());
            }
            anyhow::bail!("srpm: no source archives found. Enable the source stage first.");
        }
        if source_archives.len() > 1 {
            anyhow::bail!(
                "srpm: multiple source archives found ({}). Expected exactly one.",
                source_archives.len()
            );
        }

        // Resolve the `%files` binary→install-path map (see `resolve_bins`).
        // Computed here, before the mutable template-var borrows below, so the
        // immutable `ctx.artifacts` read does not overlap them.
        let effective_bins: BTreeMap<String, String> = resolve_bins(
            srpm_cfg.bins.as_ref(),
            ctx.artifacts
                .by_kind(ArtifactKind::Binary)
                .iter()
                .filter_map(|a| a.extra_binary()),
        );

        let source_archive = &source_archives[0];
        // Template-render `package_name` so users can reference template
        // vars (e.g. `{{ ProjectName }}` or `{{ .Env.PKG_OVERRIDE }}`).
        // Without rendering, a literal template string reaches the
        // .src.rpm filename and the spec file's `Name:` field, producing
        // an unbuildable rpm.
        let package_name_raw = srpm_cfg.package_name.as_deref().unwrap_or(&project_name);
        let package_name_rendered = ctx
            .render_template(package_name_raw)
            .with_context(|| format!("srpm: render package_name '{package_name_raw}'"))?;
        let package_name = package_name_rendered.as_str();

        // Read and render the spec file template
        let spec_file = srpm_cfg.spec_file.as_deref().unwrap_or({
            // No spec file configured — we'll generate a minimal one
            ""
        });

        let spec_contents = if spec_file.is_empty() {
            // Generate a minimal spec file
            generate_default_spec(
                package_name,
                &version,
                &srpm_cfg,
                &source_archive.name,
                &effective_bins,
                ctx.env_source(),
            )
        } else {
            // Read the user-provided spec template and render it
            let template = fs::read_to_string(spec_file)
                .with_context(|| format!("srpm: read spec file '{}'", spec_file))?;

            // Set SRPM-specific template vars
            ctx.template_vars_mut().set("PackageName", package_name);
            ctx.template_vars_mut().set("Source", &source_archive.name);
            if let Some(ref summary) = srpm_cfg.summary {
                ctx.template_vars_mut().set("Summary", summary);
            }
            if let Some(ref group) = srpm_cfg.group {
                ctx.template_vars_mut().set("Group", group);
            }
            if let Some(ref license) = srpm_cfg.license {
                ctx.template_vars_mut().set("License", license);
            }
            if let Some(ref url) = srpm_cfg.url {
                ctx.template_vars_mut().set("URL", url);
            }
            if let Some(ref description) = srpm_cfg.description {
                ctx.template_vars_mut().set("Description", description);
            }
            if let Some(ref maintainer) = srpm_cfg.maintainer {
                ctx.template_vars_mut().set("Maintainer", maintainer);
            }
            if let Some(ref vendor) = srpm_cfg.vendor {
                ctx.template_vars_mut().set("Vendor", vendor);
            }
            if let Some(ref packager) = srpm_cfg.packager {
                ctx.template_vars_mut().set("Packager", packager);
            }
            // Surface the optional RPM-spec fields as template vars so
            // user-supplied spec files can reference them with `{{ .Foo }}`.
            if let Some(ref build_host) = srpm_cfg.build_host {
                ctx.template_vars_mut().set("BuildHost", build_host);
            }
            if let Some(ref prerelease) = srpm_cfg.prerelease {
                ctx.template_vars_mut().set("Prerelease", prerelease);
            }
            if let Some(ref version_metadata) = srpm_cfg.version_metadata {
                ctx.template_vars_mut()
                    .set("VersionMetadata", version_metadata);
            }
            if let Some(ref pretrans) = srpm_cfg.pretrans {
                ctx.template_vars_mut().set("Pretrans", pretrans);
            }
            if let Some(ref posttrans) = srpm_cfg.posttrans {
                ctx.template_vars_mut().set("Posttrans", posttrans);
            }
            if let Some(prefixes) = srpm_cfg.prefixes.as_deref()
                && !prefixes.is_empty()
            {
                // Concatenate one Prefix: per line so the spec template can
                // splat the value verbatim — matches `Prefix:` directive
                // semantics in RPM headers.
                let joined = prefixes
                    .iter()
                    .map(|p| format!("Prefix: {p}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                ctx.template_vars_mut().set("Prefixes", &joined);
            }
            // Expose `Bins` as a structured binary→install-path map so
            // user spec templates can range over it
            // (`{% for bin, path in Bins %}`), mirroring GR's
            // `map[string]string` template field.
            if !effective_bins.is_empty() {
                let map: serde_json::Map<String, serde_json::Value> = effective_bins
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect();
                ctx.template_vars_mut()
                    .set_structured("Bins", serde_json::Value::Object(map));
            }

            ctx.render_template(&template)
                .with_context(|| format!("srpm: render spec template '{}'", spec_file))?
        };

        // Determine output filename
        let file_name_template = srpm_cfg
            .file_name_template
            .as_deref()
            .unwrap_or("{{ PackageName }}-{{ Version }}.src.rpm");

        ctx.template_vars_mut().set("PackageName", package_name);

        let package_filename = ctx
            .render_template(file_name_template)
            .with_context(|| "srpm: render file_name_template")?;
        let package_filename = if package_filename.ends_with(".src.rpm") {
            package_filename
        } else {
            format!("{}.src.rpm", package_filename)
        };

        if dry_run {
            log.status(&format!(
                "(dry-run) would create source RPM: {}",
                package_filename
            ));
            return Ok(());
        }

        // Write spec file
        let spec_path = dist.join(format!("{}.srpms.spec", package_name));
        fs::create_dir_all(&dist)
            .with_context(|| format!("srpm: create dist dir {}", dist.display()))?;
        fs::write(&spec_path, &spec_contents)
            .with_context(|| format!("srpm: write spec file {}", spec_path.display()))?;

        log.status(&format!("creating source RPM: {}", package_filename));

        // Build the SRPM using rpmbuild -bs
        let srpm_path = dist.join(&package_filename);

        // Create rpmbuild directory structure
        let rpmbuild_dir = dist.join("rpmbuild");
        let sources_dir = rpmbuild_dir.join("SOURCES");
        let specs_dir = rpmbuild_dir.join("SPECS");
        let srpms_dir = rpmbuild_dir.join("SRPMS");
        for dir in &[&sources_dir, &specs_dir, &srpms_dir] {
            fs::create_dir_all(dir)?;
        }

        // Copy source archive to SOURCES
        fs::copy(&source_archive.path, sources_dir.join(&source_archive.name))
            .with_context(|| "srpm: copy source archive to rpmbuild SOURCES")?;

        // Copy spec file to SPECS
        let spec_dest = specs_dir.join(format!("{}.spec", package_name));
        fs::copy(&spec_path, &spec_dest).with_context(|| "srpm: copy spec to rpmbuild SPECS")?;

        // Resolve signature configuration (GoReleaser parity: skip_sign + SRPM_PASSPHRASE)
        let effective_signature = if skip_sign {
            None
        } else {
            srpm_cfg.signature.as_ref()
        };

        // Run rpmbuild
        let mut rpmbuild_cmd = Command::new("rpmbuild");
        rpmbuild_cmd
            .arg("-bs")
            .arg("--define")
            .arg(format!("_topdir {}", rpmbuild_dir.display()));

        // Wire signing options when signature config is present
        if let Some(sig) = effective_signature
            && let Some(ref key_file) = sig.key_file
        {
            rpmbuild_cmd
                .arg("--define")
                .arg(format!("_gpg_name {}", key_file));
            rpmbuild_cmd.arg("--sign");

            // read SRPM_PASSPHRASE env var when no
            // passphrase is configured inline.
            if let Some(ref passphrase) = sig.key_passphrase {
                rpmbuild_cmd.env("GPG_PASSPHRASE", passphrase);
            } else if let Some(passphrase) = ctx.env_var("SRPM_PASSPHRASE")
                && !passphrase.is_empty()
            {
                rpmbuild_cmd.env("GPG_PASSPHRASE", &passphrase);
            }
        }

        rpmbuild_cmd.arg(&spec_dest);
        let output = rpmbuild_cmd
            .output()
            .with_context(|| "srpm: failed to spawn rpmbuild")?;

        // Route through the logger so stderr/stdout are passed through
        // env-driven redaction before they reach the error chain. rpmbuild
        // echoes GPG_PASSPHRASE / SRPM_PASSPHRASE on signing failure.
        log.check_output(output, "rpmbuild -bs")?;

        // Find the generated SRPM in SRPMS/
        let generated: Vec<PathBuf> = glob::glob(&format!("{}/**/*.src.rpm", srpms_dir.display()))
            .into_iter()
            .flat_map(|entries| entries.filter_map(|e| e.ok()))
            .collect();

        let generated_path = generated.first().ok_or_else(|| {
            anyhow::anyhow!("srpm: rpmbuild succeeded but no .src.rpm found in SRPMS/")
        })?;

        // Move to dist with the desired filename
        fs::copy(generated_path, &srpm_path).with_context(|| {
            format!(
                "srpm: copy {} -> {}",
                generated_path.display(),
                srpm_path.display()
            )
        })?;

        // Register artifact
        let mut metadata = HashMap::new();
        // Mirrors GR `internal/pipe/srpm/srpm.go` (commit e696cf8):
        //   ExtraFormat: strings.TrimPrefix(extension, ".") == "src.rpm"
        //   ExtraExt:    extension                          == ".src.rpm"
        // Downstream template consumers (`.Artifact.Format`, `.Artifact.Ext`)
        // and other stages keying off the canonical extension see the same
        // values regardless of whether the artifact came from the archive
        // stage or the SRPM stage.
        metadata.insert("format".to_string(), "src.rpm".to_string());
        metadata.insert("ext".to_string(), ".src.rpm".to_string());

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::SourceRpm,
            name: package_filename,
            path: srpm_path,
            target: None,
            crate_name: project_name,
            metadata,
            size: None,
        });

        Ok(())
    }
}

/// Resolve the effective `bins` map (binary name → `%files` install path).
///
/// A user-supplied `bins:` map (`override_bins`) is authoritative and is
/// returned verbatim. When absent, every binary the build produced
/// (`built_binaries`) defaults to `%{_bindir}/<name>` (i.e. `/usr/bin/<name>`,
/// the RPM-idiomatic install location for a built binary). Mirrors GR's
/// `SRPM.Bins` default, with `%{_bindir}/<name>` substituted for Go's
/// `%{goipath}` since Rust has no import path.
///
/// The source RPM is project-global (one `.spec` for the whole repo), so the
/// caller passes ALL produced binaries across every crate. Collecting into a
/// `BTreeMap` dedupes a binary that appears under multiple build targets to a
/// single entry and yields deterministic key order.
fn resolve_bins(
    override_bins: Option<&BTreeMap<String, String>>,
    built_binaries: impl Iterator<Item = String>,
) -> BTreeMap<String, String> {
    if let Some(bins) = override_bins {
        return bins.clone();
    }
    built_binaries
        .map(|name| {
            let path = format!("%{{_bindir}}/{name}");
            (name, path)
        })
        .collect()
}

/// Generate a minimal RPM spec file when no user template is provided.
///
/// Folds in every SrpmConfig field so that
/// `spec_file:` and the auto-generated path produce semantically equivalent
/// output for the new fields:
///
/// - `prerelease` / `version_metadata` → suffixed onto `Version:` (e.g.
///   `1.2.3~rc1+g1234abc`).
/// - `prefixes` → emitted as one `Prefix:` directive per entry (RPM's tag
///   for relocatable installs).
/// - `build_host` → emitted as a `BuildHost:` tag override.
/// - `pretrans` / `posttrans` → inlined as `%pretrans` / `%posttrans`
///   scriptlets that source the configured script file at install time.
/// - `bins` (the resolved binary→install-path map passed in `bins`) →
///   emitted as real `%files` ownership entries (one install path per
///   line), declaring which installed files the package owns. Mirrors
///   GR `SRPM.Bins`, whose values feed the spec's `%files` section.
fn generate_default_spec(
    package_name: &str,
    version: &str,
    cfg: &SrpmConfig,
    source_name: &str,
    bins: &BTreeMap<String, String>,
    env: &dyn anodizer_core::env_source::EnvSource,
) -> String {
    let summary = cfg.summary.as_deref().unwrap_or(package_name);
    let license = cfg.license.as_deref().unwrap_or("MIT");
    let url = cfg.url.as_deref().unwrap_or("");
    let description = cfg.description.as_deref().unwrap_or(package_name);

    // Compose the version string with prerelease (~suffix) and version
    // metadata (+suffix) per the GR-aligned SrpmConfig contract.
    let version_field = {
        let mut out = version.to_string();
        if let Some(pre) = cfg.prerelease.as_deref() {
            out.push('~');
            out.push_str(pre);
        }
        if let Some(meta) = cfg.version_metadata.as_deref() {
            out.push('+');
            out.push_str(meta);
        }
        out
    };

    let maintainer = cfg.maintainer.as_deref().unwrap_or(package_name);

    // Optional header tags / comments — emit only when configured.
    let mut header_extras = String::new();
    if let Some(epoch) = cfg.epoch.as_deref()
        && !epoch.is_empty()
    {
        // `Epoch:` is load-bearing for upgrade ordering when users
        // migrate from a `1:x.y.z`-style version scheme. Silently
        // dropping it lets rpm compute the wrong "newer than" order
        // during `dnf upgrade`, pinning operators on an old release
        // they can't push past without manual epoch surgery.
        header_extras.push_str(&format!("Epoch:          {epoch}\n"));
    }
    if let Some(group) = cfg.group.as_deref() {
        header_extras.push_str(&format!("Group:           {group}\n"));
    }
    if let Some(section) = cfg.section.as_deref() {
        // `section` is the deb-style classification; rpm has no native
        // equivalent so surface it as a header comment that downstream
        // tooling scanning for it can pick up.
        header_extras.push_str(&format!("# Section: {section}\n"));
    }
    if let Some(vendor) = cfg.vendor.as_deref() {
        header_extras.push_str(&format!("Vendor:         {vendor}\n"));
    }
    if let Some(packager) = cfg.packager.as_deref() {
        header_extras.push_str(&format!("Packager:       {packager}\n"));
    }
    if let Some(host) = cfg.build_host.as_deref() {
        header_extras.push_str(&format!("BuildHost:      {host}\n"));
    }
    if let Some(prefixes) = cfg.prefixes.as_deref() {
        for p in prefixes {
            header_extras.push_str(&format!("Prefix:         {p}\n"));
        }
    }

    // Compression macro — `compression: zstd` (or `xz` / `gzip` / `none`)
    // injects rpmbuild's `_source_payload` + `_source_compressor` macros
    // so the .src.rpm payload is encoded with the requested algorithm
    // instead of rpmbuild's gzip default. The `w19.zstdio` /
    // `w7.xzdio` / `w9.gzdio` / `w0.gzdio` (none → gzip level 0) syntax
    // is the rpm idiom; users who set `compression:` expect the file
    // size on disk to reflect their choice.
    let mut compression_macros = String::new();
    if let Some(comp) = cfg.compression.as_deref() {
        let lower = comp.to_ascii_lowercase();
        let (payload, compressor): (String, String) = match lower.as_str() {
            "zstd" => ("w19.zstdio".into(), "zstd".into()),
            "xz" => ("w7.xzdio".into(), "xz".into()),
            "gzip" => ("w9.gzdio".into(), "gzip".into()),
            "none" => ("w0.gzdio".into(), "gzip".into()),
            // Unknown values pass through verbatim — preserves forward-
            // compat with new rpm payload codecs without requiring a
            // stage rebuild. Owned Strings avoid the Box::leak footgun
            // that would grow heap-permanently per call.
            other => (format!("w9.{other}io"), other.to_string()),
        };
        compression_macros.push_str(&format!(
            "%define _source_payload      {payload}\n%define _source_compressor   {compressor}\n\n"
        ));
    }

    // %files — emit `%doc` lines per configured doc path plus a
    // `%license` line for the license file. Both are honored by
    // rpmbuild's `%install` machinery when the corresponding files
    // exist in the build root. Without these the README / LICENSE /
    // CHANGELOG never make it into the .src.rpm payload even when the
    // user explicitly listed them.
    let mut files_block = String::new();
    if let Some(license_name) = cfg.license_file_name.as_deref() {
        files_block.push_str(&format!("%license {license_name}\n"));
    }
    // Binary ownership entries — one install path per binary, in
    // deterministic key order (BTreeMap). These declare which installed
    // files the package owns (GR `SRPM.Bins` semantics). The map values
    // are the install paths; the keys (binary names) are not emitted
    // verbatim because RPM `%files` lists paths, not logical names.
    for install_path in bins.values() {
        files_block.push_str(&format!("{install_path}\n"));
    }
    if let Some(docs) = cfg.docs.as_deref() {
        for d in docs {
            files_block.push_str(&format!("%doc {d}\n"));
        }
    }
    // Extra `contents` entries → `Source<N>:` declarations + `%doc`
    // entries when content_type == "doc". The spec author still owns
    // `%install`, but the source-files are declared so rpmbuild can
    // pull them into the SRPM payload.
    let mut extra_sources = String::new();
    if let Some(contents) = cfg.contents.as_deref() {
        for (src_idx, entry) in (1_u32..).zip(contents.iter()) {
            extra_sources.push_str(&format!("Source{src_idx}:        {src}\n", src = entry.src));
            let is_doc = entry
                .content_type
                .as_deref()
                .map(|t| t.eq_ignore_ascii_case("doc"))
                .unwrap_or(false);
            if is_doc {
                files_block.push_str(&format!("%doc {dst}\n", dst = entry.dst));
            }
        }
    }

    // Optional scriptlets — emit a `%pretrans` / `%posttrans` block that
    // sources the configured file at install time.
    let mut scriptlets = String::new();
    if let Some(pretrans) = cfg.pretrans.as_deref() {
        scriptlets.push_str(&format!("\n%pretrans\n. {pretrans}\n"));
    }
    if let Some(posttrans) = cfg.posttrans.as_deref() {
        scriptlets.push_str(&format!("\n%posttrans\n. {posttrans}\n"));
    }

    format!(
        r#"{compression_macros}Name:           {package_name}
Version:        {version_field}
Release:        1%{{?dist}}
Summary:        {summary}
License:        {license}
URL:            {url}
Source0:        {source_name}
{extra_sources}{header_extras}
%description
{description}

%prep
%autosetup

%build

%install

%files
{files_block}{scriptlets}
%changelog
* {date} {maintainer} - {version_field}-1
- Release {version_field}
"#,
        // SDE-aware: honor SOURCE_DATE_EPOCH so the spec's %changelog
        // header is byte-stable across reproducible-build runs. Wall-
        // clock fallback when SDE is unset matches the legacy behavior.
        date = anodizer_core::sde::resolve_now_with_env(env).format("%a %b %d %Y"),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_srpm_stage_skips_when_not_enabled() {
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        let stage = SrpmStage;
        // No srpm config → no-op
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_srpm_stage_skips_when_disabled() {
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        ctx.config.srpms = Some(SrpmConfig {
            enabled: Some(false),
            ..Default::default()
        });
        let stage = SrpmStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_srpm_requires_source_archive() {
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        ctx.config.srpms = Some(SrpmConfig {
            enabled: Some(true),
            ..Default::default()
        });
        let stage = SrpmStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no source archives"),
            "should require source archive"
        );
    }

    /// `format` and `ext` extras mirror GoReleaser's
    /// `artifact.ExtraFormat: "src.rpm"` / `artifact.ExtraExt: ".src.rpm"` so
    /// downstream filename templates and publisher stages see the canonical
    /// extension and routing key. Because the artifact emission path runs
    /// `rpmbuild` (an external tool unavailable in CI), this regression test
    /// pins the literals at source level rather than driving the full pipe
    /// end-to-end.
    #[test]
    fn test_srpm_artifact_metadata_includes_format_and_ext() {
        let src = include_str!("lib.rs");
        assert!(
            src.contains("metadata.insert(\"ext\".to_string(), \".src.rpm\".to_string())"),
            "srpm artifact must register `ext` metadata with the canonical \
             `.src.rpm` extension (GR `artifact.ExtraExt` parity)"
        );
        assert!(
            src.contains("metadata.insert(\"format\".to_string(), \"src.rpm\".to_string())"),
            "srpm artifact must register `format=src.rpm` (GR \
             `artifact.ExtraFormat` parity — commit e696cf8 sets \
             `strings.TrimPrefix(extension, \".\")`)"
        );
    }

    #[test]
    fn test_generate_default_spec() {
        let cfg = SrpmConfig {
            summary: Some("A test package".to_string()),
            license: Some("Apache-2.0".to_string()),
            url: Some("https://example.com".to_string()),
            description: Some("Test description".to_string()),
            ..Default::default()
        };
        let spec = generate_default_spec(
            "myapp",
            "1.0.0",
            &cfg,
            "myapp-1.0.0.tar.gz",
            &BTreeMap::new(),
            &anodizer_core::env_source::MapEnvSource::new(),
        );
        assert!(spec.contains("Name:           myapp"));
        assert!(spec.contains("Version:        1.0.0"));
        assert!(spec.contains("Summary:        A test package"));
        assert!(spec.contains("License:        Apache-2.0"));
        assert!(spec.contains("Source0:        myapp-1.0.0.tar.gz"));
    }

    // The optional RPM-spec fields (prerelease/version_metadata/prefixes/
    // build_host/pretrans/posttrans/bins) must be folded into the
    // auto-generated default spec, not only into the user-supplied
    // `spec_file:` template surface.
    /// `generate_default_spec` must honor `SOURCE_DATE_EPOCH` for the
    /// `%changelog` header date — without this, two from-clean
    /// determinism-harness rebuilds emit `*.spec` files with different
    /// `* <date> ...` lines, drifting the SRPM and every downstream
    /// archive that bundles it.
    #[test]
    fn test_generate_default_spec_honors_sde_for_changelog_date() {
        let cfg = SrpmConfig::default();
        let env =
            anodizer_core::env_source::MapEnvSource::new().with("SOURCE_DATE_EPOCH", "1715000000");
        let spec = generate_default_spec(
            "myapp",
            "1.0.0",
            &cfg,
            "myapp-1.0.0.tar.gz",
            &BTreeMap::new(),
            &env,
        );
        // 1715000000 → 2024-05-06 Mon (UTC).
        assert!(
            spec.contains("* Mon May 06 2024"),
            "spec %changelog must use SDE-derived date; got:\n{spec}"
        );
    }

    #[test]
    fn test_generate_default_spec_emits_new_rpm_fields() {
        let cfg = SrpmConfig {
            prerelease: Some("rc1".to_string()),
            version_metadata: Some("g1234abc".to_string()),
            build_host: Some("build.local".to_string()),
            prefixes: Some(vec!["/opt".to_string(), "/usr/local".to_string()]),
            pretrans: Some("scripts/pretrans.sh".to_string()),
            posttrans: Some("scripts/posttrans.sh".to_string()),
            ..Default::default()
        };
        let spec = generate_default_spec(
            "myapp",
            "1.0.0",
            &cfg,
            "myapp-1.0.0.tar.gz",
            &BTreeMap::new(),
            &anodizer_core::env_source::MapEnvSource::new(),
        );
        // Version field carries prerelease (~) and metadata (+) suffixes.
        assert!(
            spec.contains("Version:        1.0.0~rc1+g1234abc"),
            "version must include prerelease + metadata; got:\n{spec}"
        );
        // Build host emitted as RPM tag override.
        assert!(spec.contains("BuildHost:      build.local"));
        // Each prefix becomes its own `Prefix:` directive.
        assert!(spec.contains("Prefix:         /opt"));
        assert!(spec.contains("Prefix:         /usr/local"));
        // Pretrans + posttrans scriptlets sourcing the configured files.
        assert!(spec.contains("%pretrans\n. scripts/pretrans.sh"));
        assert!(spec.contains("%posttrans\n. scripts/posttrans.sh"));
    }

    /// An explicit `bins:` override is emitted verbatim into the `%files`
    /// section (binary→install-path map, GR `SRPM.Bins` semantics), in
    /// deterministic key order.
    #[test]
    fn test_generate_default_spec_emits_bins_override_in_files() {
        let mut bins = BTreeMap::new();
        bins.insert("myapp".to_string(), "/opt/myapp/bin/myapp".to_string());
        bins.insert(
            "myapp-helper".to_string(),
            "%{_bindir}/myapp-helper".to_string(),
        );
        let cfg = SrpmConfig {
            bins: Some(bins.clone()),
            ..Default::default()
        };
        // `bins` passed to generate_default_spec is the already-resolved map,
        // which for an override equals the config map.
        let resolved = resolve_bins(cfg.bins.as_ref(), std::iter::empty());
        assert_eq!(resolved, bins, "override must pass through verbatim");
        let spec = generate_default_spec(
            "myapp",
            "1.0.0",
            &cfg,
            "myapp-1.0.0.tar.gz",
            &resolved,
            &anodizer_core::env_source::MapEnvSource::new(),
        );
        // Both install paths land in %files, sorted by binary name
        // (myapp before myapp-helper).
        let files_idx = spec.find("%files").expect("spec has %files");
        let files_tail = &spec[files_idx..];
        assert!(
            files_tail.contains("/opt/myapp/bin/myapp\n"),
            "override install path must appear in %files; got:\n{spec}"
        );
        assert!(
            files_tail.contains("%{_bindir}/myapp-helper\n"),
            "second override install path must appear in %files; got:\n{spec}"
        );
        let a = files_tail.find("/opt/myapp/bin/myapp").unwrap();
        let b = files_tail.find("%{_bindir}/myapp-helper").unwrap();
        assert!(a < b, "%files entries must be in deterministic key order");
    }

    /// When `bins:` is omitted, each binary the build produced for the crate
    /// defaults to `%{_bindir}/<name>` (`/usr/bin/<name>`). Derived from the
    /// per-crate binary names — no user config required.
    #[test]
    fn test_resolve_bins_derives_default_bindir_paths() {
        let built = vec!["myapp".to_string(), "myapp-helper".to_string()];
        let resolved = resolve_bins(None, built.into_iter());
        let mut expected = BTreeMap::new();
        expected.insert("myapp".to_string(), "%{_bindir}/myapp".to_string());
        expected.insert(
            "myapp-helper".to_string(),
            "%{_bindir}/myapp-helper".to_string(),
        );
        assert_eq!(resolved, expected);

        // …and those default paths reach the %files section.
        let cfg = SrpmConfig::default();
        let spec = generate_default_spec(
            "myapp",
            "1.0.0",
            &cfg,
            "myapp-1.0.0.tar.gz",
            &resolved,
            &anodizer_core::env_source::MapEnvSource::new(),
        );
        let files_tail = &spec[spec.find("%files").unwrap()..];
        assert!(files_tail.contains("%{_bindir}/myapp\n"), "got:\n{spec}");
        assert!(
            files_tail.contains("%{_bindir}/myapp-helper\n"),
            "got:\n{spec}"
        );
    }

    /// Add a `Binary` artifact carrying the `binary` metadata key that
    /// `extra_binary()` reads, under the given crate name + target. Mirrors
    /// how the build stage registers binaries so the test drives the real
    /// `ctx.artifacts.by_kind(Binary)` query the stage runs.
    #[cfg(test)]
    fn add_binary(ctx: &mut Context, crate_name: &str, bin: &str, target: &str) {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from(format!("dist/{target}/{bin}")),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: HashMap::from([("binary".to_string(), bin.to_string())]),
            size: None,
        });
    }

    /// The default `%files` derivation runs the SAME project-global
    /// `ctx.artifacts.by_kind(Binary)` query the stage uses — NOT a stubbed
    /// iterator. The source RPM is project-global, so binaries from EVERY
    /// crate land in the one `.spec` regardless of crate identity. The same
    /// binary across two targets dedupes to one entry, in deterministic key
    /// order.
    #[test]
    fn test_default_bins_derive_project_global_from_ctx_artifacts() {
        // project_name is set distinct from every crate name because the
        // default %files must derive from all produced binaries regardless of
        // crate identity; a crate-name-scoped lookup would match nothing here.
        let config = anodizer_core::config::Config {
            project_name: "myproject".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());

        // Two distinct crates; `cli` builds the same binary for two targets
        // (must dedupe), `daemon` builds its own.
        add_binary(&mut ctx, "cli", "alpha", "x86_64-unknown-linux-gnu");
        add_binary(&mut ctx, "cli", "alpha", "aarch64-unknown-linux-gnu");
        add_binary(&mut ctx, "daemon", "beta", "x86_64-unknown-linux-gnu");

        // Exactly the expression the stage runs to build `effective_bins`.
        let effective_bins: BTreeMap<String, String> = resolve_bins(
            None,
            ctx.artifacts
                .by_kind(ArtifactKind::Binary)
                .iter()
                .filter_map(|a| a.extra_binary()),
        );

        // Both crates' binaries present, deduped to one entry each.
        let mut expected = BTreeMap::new();
        expected.insert("alpha".to_string(), "%{_bindir}/alpha".to_string());
        expected.insert("beta".to_string(), "%{_bindir}/beta".to_string());
        assert_eq!(
            effective_bins, expected,
            "project-global default must own every produced binary, deduped"
        );

        let spec = generate_default_spec(
            "myproject",
            "1.0.0",
            &SrpmConfig::default(),
            "myproject-1.0.0.tar.gz",
            &effective_bins,
            &anodizer_core::env_source::MapEnvSource::new(),
        );
        let files = &spec[spec.find("%files").unwrap()..];
        assert!(files.contains("%{_bindir}/alpha\n"), "got:\n{spec}");
        assert!(files.contains("%{_bindir}/beta\n"), "got:\n{spec}");
        // alpha before beta — deterministic key order, single alpha entry.
        let a = files.find("%{_bindir}/alpha").unwrap();
        let b = files.find("%{_bindir}/beta").unwrap();
        assert!(a < b, "%files entries must be in deterministic key order");
        assert_eq!(
            files.matches("%{_bindir}/alpha\n").count(),
            1,
            "alpha built for two targets must appear once; got:\n{spec}"
        );
    }

    /// Single-crate positive case: one crate, one binary, default derives
    /// `%{_bindir}/<name>` from the real artifact query.
    #[test]
    fn test_default_bins_single_crate_from_ctx_artifacts() {
        let config = anodizer_core::config::Config {
            project_name: "solo".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        add_binary(&mut ctx, "solo", "solo", "x86_64-unknown-linux-gnu");

        let effective_bins: BTreeMap<String, String> = resolve_bins(
            None,
            ctx.artifacts
                .by_kind(ArtifactKind::Binary)
                .iter()
                .filter_map(|a| a.extra_binary()),
        );
        let mut expected = BTreeMap::new();
        expected.insert("solo".to_string(), "%{_bindir}/solo".to_string());
        assert_eq!(effective_bins, expected);
    }

    /// Cover the six optional fields surfaced through
    /// `generate_default_spec`: epoch, section, compression, docs,
    /// contents, license_file_name. Each emits the GR/rpm-idiom shape
    /// that downstream tooling expects to see.
    #[test]
    fn test_generate_default_spec_emits_optional_fields() {
        use anodizer_core::config::NfpmContent;
        let cfg = SrpmConfig {
            epoch: Some("1".to_string()),
            section: Some("utils".to_string()),
            group: Some("Development/Tools".to_string()),
            vendor: Some("Acme".to_string()),
            packager: Some("Acme Build <build@acme.test>".to_string()),
            compression: Some("zstd".to_string()),
            license_file_name: Some("LICENSE".to_string()),
            docs: Some(vec!["README.md".to_string(), "CHANGELOG.md".to_string()]),
            contents: Some(vec![
                NfpmContent {
                    src: "extra/policy.txt".to_string(),
                    dst: "/usr/share/doc/myapp/policy.txt".to_string(),
                    content_type: Some("doc".to_string()),
                    file_info: None,
                    packager: None,
                    expand: None,
                },
                NfpmContent {
                    src: "extra/data.bin".to_string(),
                    dst: "/usr/share/myapp/data.bin".to_string(),
                    content_type: None,
                    file_info: None,
                    packager: None,
                    expand: None,
                },
            ]),
            ..Default::default()
        };
        let spec = generate_default_spec(
            "myapp",
            "1.0.0",
            &cfg,
            "myapp-1.0.0.tar.gz",
            &BTreeMap::new(),
            &anodizer_core::env_source::MapEnvSource::new(),
        );

        // epoch — upgrade-ordering tag.
        assert!(
            spec.contains("Epoch:          1"),
            "epoch must emit as RPM Epoch:; got:\n{spec}"
        );
        // section — surfaced as header comment (no native rpm tag).
        assert!(spec.contains("# Section: utils"), "got:\n{spec}");
        // group + vendor + packager — proper RPM tags.
        assert!(spec.contains("Group:           Development/Tools"));
        assert!(spec.contains("Vendor:         Acme"));
        assert!(spec.contains("Packager:       Acme Build <build@acme.test>"));
        // compression — rpm macros that swap the source-payload codec.
        assert!(
            spec.contains("%define _source_payload      w19.zstdio")
                && spec.contains("%define _source_compressor   zstd"),
            "compression: zstd must emit _source_payload + _source_compressor; got:\n{spec}"
        );
        // license_file_name + docs — %files entries.
        assert!(spec.contains("%license LICENSE"));
        assert!(spec.contains("%doc README.md"));
        assert!(spec.contains("%doc CHANGELOG.md"));
        // contents — Source<N>: declarations for each entry, %doc for
        // type=doc entries.
        assert!(spec.contains("Source1:        extra/policy.txt"));
        assert!(spec.contains("Source2:        extra/data.bin"));
        assert!(
            spec.contains("%doc /usr/share/doc/myapp/policy.txt"),
            "contents[type=doc] must add %doc <dst>; got:\n{spec}"
        );
    }

    /// Unknown compression values pass through verbatim — preserves
    /// forward-compat with new rpm payload codecs without requiring a
    /// stage rebuild.
    #[test]
    fn test_generate_default_spec_unknown_compression_passes_through() {
        let cfg = SrpmConfig {
            compression: Some("lz4".to_string()),
            ..Default::default()
        };
        let spec = generate_default_spec(
            "myapp",
            "1.0.0",
            &cfg,
            "myapp-1.0.0.tar.gz",
            &BTreeMap::new(),
            &anodizer_core::env_source::MapEnvSource::new(),
        );
        assert!(spec.contains("%define _source_payload      w9.lz4io"));
        assert!(spec.contains("%define _source_compressor   lz4"));
    }

    #[test]
    fn test_srpm_config_parsing() {
        use anodizer_core::config::Config;

        let yaml = r#"
project_name: test
srpm:
  enabled: true
  package_name: myapp
  spec_file: myapp.spec
  summary: "My application"
  license: MIT
  url: "https://example.com"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let srpm = config.srpms.as_ref().unwrap();
        assert_eq!(srpm.enabled, Some(true));
        assert_eq!(srpm.package_name.as_deref(), Some("myapp"));
        assert_eq!(srpm.spec_file.as_deref(), Some("myapp.spec"));
        assert_eq!(srpm.summary.as_deref(), Some("My application"));
    }

    #[test]
    fn test_srpm_new_rpm_spec_fields_parse() {
        // The optional RPM-spec fields (prerelease/version_metadata/prefixes/
        // build_host/pretrans/posttrans/bins) parse and surface on the
        // SrpmConfig struct. `bins` is a binary→install-path map.
        use anodizer_core::config::Config;

        let yaml = r#"
project_name: test
srpm:
  enabled: true
  package_name: myapp
  bins:
    myapp-cli: "%{_bindir}/myapp-cli"
  prefixes:
    - /opt/myapp
  build_host: build.local
  pretrans: scripts/pretrans.sh
  posttrans: scripts/posttrans.sh
  prerelease: rc1
  version_metadata: g1234abc
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let srpm = config.srpms.as_ref().unwrap();
        let bins = srpm.bins.as_ref().unwrap();
        assert_eq!(
            bins.get("myapp-cli").map(String::as_str),
            Some("%{_bindir}/myapp-cli")
        );
        assert_eq!(
            srpm.prefixes.as_ref().unwrap(),
            &vec!["/opt/myapp".to_string()]
        );
        assert_eq!(srpm.build_host.as_deref(), Some("build.local"));
        assert_eq!(srpm.pretrans.as_deref(), Some("scripts/pretrans.sh"));
        assert_eq!(srpm.posttrans.as_deref(), Some("scripts/posttrans.sh"));
        assert_eq!(srpm.prerelease.as_deref(), Some("rc1"));
        assert_eq!(srpm.version_metadata.as_deref(), Some("g1234abc"));
    }
}
