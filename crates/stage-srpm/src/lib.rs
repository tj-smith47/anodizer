use std::collections::HashMap;
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
            generate_default_spec(package_name, &version, &srpm_cfg, &source_archive.name)
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
            if let Some(ref import_path) = srpm_cfg.import_path {
                ctx.template_vars_mut().set("ImportPath", import_path);
            }
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
            if let Some(bins) = srpm_cfg.bins.as_deref()
                && !bins.is_empty()
            {
                ctx.template_vars_mut().set("Bins", &bins.join(","));
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
            } else if let Ok(passphrase) = std::env::var("SRPM_PASSPHRASE")
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
        metadata.insert("format".to_string(), "srpm".to_string());
        // Mirror GoReleaser's `artifact.ExtraExt = ".src.rpm"` so downstream
        // template consumers (`.Artifact.Ext`, filename templates) and other
        // stages keying off the canonical extension see the same value
        // regardless of whether the artifact came from the archive stage or
        // the SRPM stage.
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
/// - `import_path` → added as a comment line near the header so downstream
///   tooling (vendor tooling that scans spec headers for VCS roots) sees it.
/// - `bins` → emitted as a `# Bins:` comment summarising which build IDs
///   the SRPM bundles, mirroring the spec_file template variable surface.
fn generate_default_spec(
    package_name: &str,
    version: &str,
    cfg: &SrpmConfig,
    source_name: &str,
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
    if let Some(import_path) = cfg.import_path.as_deref() {
        header_extras.push_str(&format!("# ImportPath: {import_path}\n"));
    }
    if let Some(bins) = cfg.bins.as_deref()
        && !bins.is_empty()
    {
        header_extras.push_str(&format!("# Bins: {}\n", bins.join(",")));
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
        date = anodizer_core::sde::resolve_now().format("%a %b %d %Y"),
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

    /// `ext` extra mirrors GoReleaser's
    /// `artifact.ExtraExt: ".src.rpm"` so downstream filename templates and
    /// publisher stages see the canonical extension. Because the artifact
    /// emission path runs `rpmbuild` (an external tool unavailable in CI),
    /// this regression test pins the literal at source level rather than
    /// driving the full pipe end-to-end.
    #[test]
    fn test_srpm_artifact_metadata_includes_ext() {
        let src = include_str!("lib.rs");
        assert!(
            src.contains("metadata.insert(\"ext\".to_string(), \".src.rpm\".to_string())"),
            "srpm artifact must register `ext` metadata with the canonical \
             `.src.rpm` extension (GR `artifact.ExtraExt` parity)"
        );
        assert!(
            src.contains("metadata.insert(\"format\".to_string(), \"srpm\".to_string())"),
            "srpm artifact must continue to register `format=srpm`"
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
        let spec = generate_default_spec("myapp", "1.0.0", &cfg, "myapp-1.0.0.tar.gz");
        assert!(spec.contains("Name:           myapp"));
        assert!(spec.contains("Version:        1.0.0"));
        assert!(spec.contains("Summary:        A test package"));
        assert!(spec.contains("License:        Apache-2.0"));
        assert!(spec.contains("Source0:        myapp-1.0.0.tar.gz"));
    }

    // The optional RPM-spec fields (prerelease/version_metadata/prefixes/
    // build_host/pretrans/posttrans/import_path/bins) must be folded into
    // the auto-generated default spec, not only into the user-supplied
    // `spec_file:` template surface.
    /// `generate_default_spec` must honor `SOURCE_DATE_EPOCH` for the
    /// `%changelog` header date — without this, two from-clean
    /// determinism-harness rebuilds emit `*.spec` files with different
    /// `* <date> ...` lines, drifting the SRPM and every downstream
    /// archive that bundles it.
    #[test]
    fn test_generate_default_spec_honors_sde_for_changelog_date() {
        // Serialize env mutation; cargo test runs tests in parallel
        // within a single binary, and SOURCE_DATE_EPOCH is read by other
        // code paths (e.g. populate_time_vars in core).
        let _g = anodizer_core::test_helpers::env::env_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: single-threaded section, guarded by the mutex above.
        unsafe { std::env::set_var("SOURCE_DATE_EPOCH", "1715000000") };

        let cfg = SrpmConfig::default();
        let spec = generate_default_spec("myapp", "1.0.0", &cfg, "myapp-1.0.0.tar.gz");
        // 1715000000 → 2024-05-06 Mon (UTC).
        assert!(
            spec.contains("* Mon May 06 2024"),
            "spec %changelog must use SDE-derived date; got:\n{spec}"
        );

        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
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
            import_path: Some("github.com/me/myapp".to_string()),
            bins: Some(vec!["myapp-cli".to_string()]),
            ..Default::default()
        };
        let spec = generate_default_spec("myapp", "1.0.0", &cfg, "myapp-1.0.0.tar.gz");
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
        // Import path + bins surface as header comments (mirrors spec_file
        // template-var semantics — downstream tooling can grep them out).
        assert!(spec.contains("# ImportPath: github.com/me/myapp"));
        assert!(spec.contains("# Bins: myapp-cli"));
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
        let spec = generate_default_spec("myapp", "1.0.0", &cfg, "myapp-1.0.0.tar.gz");

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
        let spec = generate_default_spec("myapp", "1.0.0", &cfg, "myapp-1.0.0.tar.gz");
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
        // build_host/pretrans/posttrans/import_path/bins) parse and surface
        // on the SrpmConfig struct.
        use anodizer_core::config::Config;

        let yaml = r#"
project_name: test
srpm:
  enabled: true
  package_name: myapp
  bins:
    - myapp-cli
  import_path: github.com/me/myapp
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
        assert_eq!(srpm.bins.as_ref().unwrap(), &vec!["myapp-cli".to_string()]);
        assert_eq!(srpm.import_path.as_deref(), Some("github.com/me/myapp"));
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
