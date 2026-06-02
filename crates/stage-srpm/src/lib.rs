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
        // RPM-safe form of the version for the user-spec template and the
        // output filename. The global `Version` var must keep the raw value
        // so downstream stages (announce/publish run after srpm) still see
        // the real version; it is swapped in only for the scoped renders below.
        let rpm_safe_version = rpm_version_field(&version);

        // Top-level directory inside the source tarball (set by the source
        // stage from the RAW version). The auto-gen spec's `%autosetup -n`
        // must target this exact dir; it is the RAW prefix, not the sanitized
        // `Version`, so a snapshot/prerelease `.src.rpm` is rebuildable.
        let source_prefix = ctx
            .template_vars()
            .get("SourcePrefix")
            .cloned()
            .unwrap_or_default();

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
        // rpmbuild expands `%{version}` in a spec's `Source0:` to the
        // sanitized `Version:` field, so the file copied into `SOURCES/` must
        // match that, not the raw-version artifact name. No-op for real
        // releases (clean version → name unchanged). Per-crate safe: both
        // `source_archive` and `version` are this published crate's.
        let rpm_source_name = rpm_source_name(&source_archive.name, &version, &rpm_safe_version);
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
            // No spec file configured — an empty path signals the
            // minimal-spec generator below.
            ""
        });

        let spec_contents = if spec_file.is_empty() {
            // Generate a minimal spec file
            generate_default_spec(
                package_name,
                &version,
                &srpm_cfg,
                &rpm_source_name,
                &source_prefix,
                &effective_bins,
                ctx.env_source(),
            )
        } else {
            // Read the user-provided spec template and render it
            let template = fs::read_to_string(spec_file)
                .with_context(|| format!("srpm: read spec file '{}'", spec_file))?;

            // Set SRPM-specific template vars
            ctx.template_vars_mut().set("PackageName", package_name);
            // Use the rpm-safe source name so a user spec's `Source0:
            // {{ Source }}` references the file actually present in SOURCES/.
            ctx.template_vars_mut().set("Source", &rpm_source_name);
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
            // (`{% for bin, path in Bins %}`), via the
            // `map[string]string` template field.
            if !effective_bins.is_empty() {
                let map: serde_json::Map<String, serde_json::Value> = effective_bins
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect();
                ctx.template_vars_mut()
                    .set_structured("Bins", serde_json::Value::Object(map));
            }

            // Render with the RPM-safe `Version` scoped in, then restore the
            // raw value so downstream stages are unaffected. The user spec
            // references `{{ Version }}` for its `Version:` field, which must
            // satisfy the RPM grammar.
            ctx.template_vars_mut().set("Version", &rpm_safe_version);
            let rendered = ctx
                .render_template(&template)
                .with_context(|| format!("srpm: render spec template '{}'", spec_file));
            ctx.template_vars_mut().set("Version", &version);
            rendered?
        };

        // Determine output filename
        let file_name_template = srpm_cfg
            .file_name_template
            .as_deref()
            .unwrap_or("{{ PackageName }}-{{ Version }}.src.rpm");

        ctx.template_vars_mut().set("PackageName", package_name);

        // The default filename template embeds `{{ Version }}`; the SRPM the
        // tool just built carries the RPM-safe version in its `Version:` tag,
        // so the artifact filename must match (and avoid an illegal `-`).
        // Scope the override and restore the raw value for downstream stages.
        ctx.template_vars_mut().set("Version", &rpm_safe_version);
        let package_filename = ctx
            .render_template(file_name_template)
            .with_context(|| "srpm: render file_name_template");
        ctx.template_vars_mut().set("Version", &version);
        let package_filename = package_filename?;
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

        // Copy source archive to SOURCES under the rpm-safe name so it
        // matches the spec's `%{version}`-expanded `Source0:` (see
        // `rpm_source_name`).
        fs::copy(&source_archive.path, sources_dir.join(&rpm_source_name))
            .with_context(|| "srpm: copy source archive to rpmbuild SOURCES")?;

        // Copy spec file to SPECS
        let spec_dest = specs_dir.join(format!("{}.spec", package_name));
        fs::copy(&spec_path, &spec_dest).with_context(|| "srpm: copy spec to rpmbuild SPECS")?;

        // Resolve signature configuration
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
        // SRPM signing resolution:
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

/// Sanitize an arbitrary version string into the RPM `Version:` grammar.
///
/// TOTAL guarantee: the output contains ONLY `[A-Za-z0-9._+~^]`, the full set
/// RPM permits in a `Version:` tag. `rpmbuild` hard-errors `Illegal char '-'`
/// (and on any other out-of-grammar byte) otherwise. Incoming versions carry
/// `-` and arbitrary characters from several sources: snapshot mode renders
/// `<base>-SNAPSHOT-<shortcommit>` (e.g. `0.5.0-SNAPSHOT-68dfcfb`), a real
/// prerelease tag arrives as e.g. `0.5.0-rc.1`, and a branch-derived
/// prerelease can carry a slash (`0.5.0-feature/x`).
///
/// Algorithm (semver-aware so the tilde lands on the prerelease separator,
/// never on a metadata dash):
/// - Split at the FIRST `+` into `head` (core + prerelease) and `tail`
///   (build metadata, possibly absent).
/// - In `head`: the prerelease separator becomes `~` (sorts BEFORE the
///   release, so a prerelease orders ahead of its final version). The
///   separator is the FIRST `-`, UNLESS a literal `~` already opened the
///   prerelease (the cfg-suffix path composes `<base>~<prerelease>`, so a
///   `-` inside that prerelease is internal, not a second separator). Once
///   the prerelease has started, any further `-` becomes `_`. Any other char
///   outside `[A-Za-z0-9._+~^]` becomes `_`.
/// - In `tail`: RPM forbids `-` in metadata too, so EVERY `-` becomes `_`
///   (no `~` — metadata must not sort before the release); any other illegal
///   char becomes `_`.
/// - Rejoin as `head + "+" + tail` when metadata was present.
///
/// This is stricter than nfpm's rpm handling, which only `replace('-','_')`s
/// the separately-configured prerelease field and leaves a metadata `-`
/// intact: anodizer neutralizes metadata dashes too, which is the RPM-correct
/// behavior.
fn rpm_version_field(version: &str) -> String {
    let legal = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '~' | '^');

    let (head, tail) = match version.split_once('+') {
        Some((h, t)) => (h, Some(t)),
        None => (version, None),
    };

    let mut out = String::with_capacity(version.len() + 1);
    // A `~` (literal in the input, e.g. the cfg-suffix path) or the first `-`
    // opens the prerelease; once open, later `-` are internal → `_`.
    let mut prerelease_started = false;
    for ch in head.chars() {
        match ch {
            '-' => {
                out.push(if prerelease_started { '_' } else { '~' });
                prerelease_started = true;
            }
            '~' => {
                out.push('~');
                prerelease_started = true;
            }
            c if legal(c) => out.push(c),
            _ => out.push('_'),
        }
    }
    if let Some(tail) = tail {
        out.push('+');
        for ch in tail.chars() {
            // `-` is illegal in RPM metadata too; collapse it (and any other
            // out-of-grammar char) to `_`. No `~`: metadata must not sort
            // ahead of the release.
            out.push(if legal(ch) { ch } else { '_' });
        }
    }
    out
}

/// Compute the source-archive filename rpmbuild will look for in `SOURCES/`.
///
/// A spec's `Source0:` commonly uses the canonical `%{name}-%{version}-…`
/// idiom; rpmbuild expands `%{version}` to the SANITIZED `Version:` field
/// (e.g. `0.5.0~SNAPSHOT_<sha>`), so the file copied into `SOURCES/` must
/// carry the sanitized version, not the raw artifact name (`0.5.0-SNAPSHOT-…`)
/// the source stage produced. Rewriting the version token to the sanitized
/// form keeps the copied file, the auto-gen spec's `Source0:`, and a user
/// spec's `{{ Source }}` all in agreement.
///
/// Contract / assumptions:
/// - Only the FIRST occurrence of `raw_version` is rewritten (`replacen`),
///   matching rpmbuild's `%{version}`, which expands the single version token
///   — not every coincidental recurrence of that string in the name.
/// - The source archive name is assumed to EMBED the version, which holds for
///   the default `name_template: "{{ ProjectName }}-{{ Version }}-source"`.
///   When it does not, the rewrite is a no-op and a `%{version}`-templated
///   `Source0:` would not resolve; such a spec must instead reference
///   `{{ Source }}` in its `Source0:`, which is always set to this same
///   reconciled name and therefore always matches the copied file. The
///   auto-gen spec path is independently self-consistent (its literal
///   `Source0:` equals this exact name regardless of embedding).
///
/// No-op for every real release: a clean version has no illegal char, so
/// `rpm_version == raw_version` and the artifact name passes through verbatim.
fn rpm_source_name(artifact_name: &str, raw_version: &str, rpm_version: &str) -> String {
    if rpm_version == raw_version {
        artifact_name.to_string()
    } else {
        artifact_name.replacen(raw_version, rpm_version, 1)
    }
}

/// Resolve the effective `bins` map (binary name → `%files` install path).
///
/// A user-supplied `bins:` map (`override_bins`) is authoritative and is
/// returned verbatim. When absent, every binary the build produced
/// (`built_binaries`) defaults to `%{_bindir}/<name>` (i.e. `/usr/bin/<name>`,
/// the RPM-idiomatic install location for a built binary). The
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
///   the binary→path map, whose values feed the spec's `%files` section.
fn generate_default_spec(
    package_name: &str,
    version: &str,
    cfg: &SrpmConfig,
    source_name: &str,
    source_prefix: &str,
    bins: &BTreeMap<String, String>,
    env: &dyn anodizer_core::env_source::EnvSource,
) -> String {
    let summary = cfg.summary.as_deref().unwrap_or(package_name);
    let license = cfg.license.as_deref().unwrap_or("MIT");
    let url = cfg.url.as_deref().unwrap_or("");
    let description = cfg.description.as_deref().unwrap_or(package_name);

    // Compose the RAW version string with prerelease (~suffix) and version
    // metadata (+suffix) per the SrpmConfig contract, THEN run a
    // single total `rpm_version_field` pass over the whole thing. Sanitizing
    // once at the end (rather than per-fragment) means a `-` anywhere — in the
    // base version (`0.5.0-rc.1`), in a configured `prerelease: rc-1`, or in
    // `version_metadata: build-7` — is neutralized to valid RPM grammar; no
    // unsanitized fragment can re-open `Illegal char '-'`.
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
        rpm_version_field(&out)
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
    // files the package owns (the binary→path map). The map values
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

    // `%autosetup` must `cd` into the exact top-level dir the source tarball
    // contains. A bare `%autosetup` defaults to `-n %{name}-%{version}`, which
    // breaks for snapshot/prerelease builds (sanitized `%{version}` ≠ the raw
    // prefix dir). Target the real prefix instead; for a prefix-less tarball
    // (empty prefix), `-c` creates the build dir and extracts the flat
    // sources into it.
    let autosetup = if source_prefix.is_empty() {
        "%autosetup -c".to_string()
    } else {
        format!("%autosetup -n {source_prefix}")
    };

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
{autosetup}

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

    /// `format` and `ext` extras follow the conventional
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
            "",
            &BTreeMap::new(),
            &anodizer_core::env_source::MapEnvSource::new(),
        );
        assert!(spec.contains("Name:           myapp"));
        assert!(spec.contains("Version:        1.0.0"));
        assert!(spec.contains("Summary:        A test package"));
        assert!(spec.contains("License:        Apache-2.0"));
        assert!(spec.contains("Source0:        myapp-1.0.0.tar.gz"));
        // Empty prefix → `%autosetup -c` (tarball has no top-level dir).
        assert!(
            spec.contains("%autosetup -c"),
            "empty prefix must emit `%autosetup -c`; got:\n{spec}"
        );
        assert!(
            !spec.contains("%autosetup -n"),
            "empty prefix must NOT emit `-n`; got:\n{spec}"
        );
    }

    /// A non-empty source prefix makes the auto-gen spec target that exact
    /// top-level dir via `%autosetup -n <prefix>`, so a snapshot/prerelease
    /// `.src.rpm` (whose tarball prefix is the RAW version) is rebuildable.
    #[test]
    fn test_generate_default_spec_autosetup_uses_source_prefix() {
        let spec = generate_default_spec(
            "anodizer",
            "0.5.0~SNAPSHOT_abc",
            &SrpmConfig::default(),
            "anodizer-0.5.0~SNAPSHOT_abc-source.tar.gz",
            // The tarball prefix is the RAW version, distinct from the
            // sanitized `Version:` above — exactly the rebuild-safe case.
            "anodizer-0.5.0-SNAPSHOT-abc",
            &BTreeMap::new(),
            &anodizer_core::env_source::MapEnvSource::new(),
        );
        assert!(
            spec.contains("%autosetup -n anodizer-0.5.0-SNAPSHOT-abc"),
            "non-empty prefix must emit `%autosetup -n <prefix>`; got:\n{spec}"
        );
        assert!(
            !spec.contains("%autosetup -c"),
            "non-empty prefix must NOT emit `-c`; got:\n{spec}"
        );
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
            "",
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
            "",
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

    /// The base version's `-` (snapshot/prerelease) must never reach the
    /// emitted `Version:` line: the first `-` becomes `~`, subsequent ones
    /// `_`, `+` metadata is untouched, and a clean version is left alone.
    /// rpmbuild rejects `-` in `Version:` outright, so this guards the
    /// auto-gen spec path against snapshot and real-RC failures alike.
    #[test]
    fn test_generate_default_spec_sanitizes_version_for_rpm_grammar() {
        let cases = [
            // (input version, expected substring on the `Version:` line)
            ("0.5.0-SNAPSHOT-68dfcfb", "0.5.0~SNAPSHOT_68dfcfb"),
            ("0.5.0-rc.1", "0.5.0~rc.1"),
            ("1.2.3+build.42", "1.2.3+build.42"),
            ("1.0.0", "1.0.0"),
            ("0.5.0-feature/x", "0.5.0~feature_x"),
        ];
        for (input, expected) in cases {
            let spec = generate_default_spec(
                "myapp",
                input,
                &SrpmConfig::default(),
                "myapp.tar.gz",
                "",
                &BTreeMap::new(),
                &anodizer_core::env_source::MapEnvSource::new(),
            );
            let version_line = spec
                .lines()
                .find(|l| l.starts_with("Version:"))
                .unwrap_or_else(|| panic!("spec has no Version: line for {input}; got:\n{spec}"));
            assert!(
                version_line.contains(expected),
                "Version: line for `{input}` must contain `{expected}`; got `{version_line}`"
            );
            assert!(
                !version_line.contains('-'),
                "Version: line for `{input}` must contain no `-` (RPM grammar); got `{version_line}`"
            );
        }
    }

    /// The sanitizer composes with the cfg `~prerelease` / `+metadata`
    /// suffixes: a base version `-` is transformed before they append, so a
    /// snapshot base + configured suffixes still yields valid RPM grammar.
    #[test]
    fn test_generate_default_spec_sanitizes_base_then_appends_suffixes() {
        let cfg = SrpmConfig {
            prerelease: Some("rc1".to_string()),
            version_metadata: Some("g1234abc".to_string()),
            ..Default::default()
        };
        let spec = generate_default_spec(
            "myapp",
            "0.5.0-SNAPSHOT-abc",
            &cfg,
            "myapp.tar.gz",
            "",
            &BTreeMap::new(),
            &anodizer_core::env_source::MapEnvSource::new(),
        );
        assert!(
            spec.contains("Version:        0.5.0~SNAPSHOT_abc~rc1+g1234abc"),
            "sanitized base must precede cfg ~prerelease/+metadata; got:\n{spec}"
        );
    }

    /// A `-` inside the CONFIGURED `prerelease` / `version_metadata` must not
    /// re-open `Illegal char '-'`: the suffixes are composed onto the raw
    /// version and the whole string is sanitized once. `rc-1` → `rc_1`,
    /// `build-7` (metadata) → `build_7`.
    #[test]
    fn test_generate_default_spec_sanitizes_dashed_cfg_suffixes() {
        let cfg = SrpmConfig {
            prerelease: Some("rc-1".to_string()),
            version_metadata: Some("build-7".to_string()),
            ..Default::default()
        };
        let spec = generate_default_spec(
            "myapp",
            "1.0.0",
            &cfg,
            "myapp.tar.gz",
            "",
            &BTreeMap::new(),
            &anodizer_core::env_source::MapEnvSource::new(),
        );
        let version_line = spec
            .lines()
            .find(|l| l.starts_with("Version:"))
            .expect("spec has Version: line");
        assert!(
            version_line.contains("1.0.0~rc_1+build_7"),
            "dashed cfg suffixes must be sanitized; got `{version_line}`"
        );
        assert!(
            !version_line.contains('-'),
            "Version: line must be `-`-free; got `{version_line}`"
        );
    }

    /// Direct unit coverage of the version sanitizer's transform rules,
    /// independent of the spec emission path. Asserts the TOTAL guarantee:
    /// only `[A-Za-z0-9._+~^]` survives, the tilde lands on the prerelease
    /// separator (never a metadata dash), and degenerate inputs are handled.
    #[test]
    fn test_rpm_version_field_transform() {
        let cases = [
            ("0.5.0-SNAPSHOT-68dfcfb", "0.5.0~SNAPSHOT_68dfcfb"),
            ("0.5.0-rc.1", "0.5.0~rc.1"),
            ("1.2.3+build.42", "1.2.3+build.42"),
            ("1.0.0", "1.0.0"),
            // Only the first `-` in head becomes `~`; the rest are `_`.
            ("1.0.0-a-b-c", "1.0.0~a_b_c"),
            // Degenerate / hostile inputs.
            ("", ""),
            ("---", "~__"),
            ("1.0.0~rc1", "1.0.0~rc1"),
            // Metadata dash must collapse to `_`, NOT `~` (no metadata sort
            // ahead of the release).
            ("1.2.3+build-7", "1.2.3+build_7"),
            // Branch-derived prerelease: slash is illegal → `_`, head dash → `~`.
            ("0.5.0-feature/x", "0.5.0~feature_x"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                rpm_version_field(input),
                expected,
                "rpm_version_field({input:?})"
            );
            // TOTAL: output is RPM-Version-grammar-legal end to end.
            assert!(
                rpm_version_field(input)
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '~' | '^')),
                "output for {input:?} must contain only RPM-legal chars"
            );
        }
    }

    /// The file copied into `SOURCES/` must carry the SANITIZED version so it
    /// matches a spec's `%{version}`-expanded `Source0:`. A snapshot-style
    /// version has its raw substring rewritten in the artifact name.
    #[test]
    fn test_rpm_source_name_rewrites_snapshot_version() {
        let raw = "0.5.0-SNAPSHOT-abc";
        let safe = rpm_version_field(raw); // 0.5.0~SNAPSHOT_abc
        assert_eq!(
            rpm_source_name("foo-0.5.0-SNAPSHOT-abc-source.tar.gz", raw, &safe),
            "foo-0.5.0~SNAPSHOT_abc-source.tar.gz"
        );
    }

    /// For a clean release version the sanitizer is a no-op, so the source
    /// name passes through verbatim — no rewrite, byte-identical artifact.
    #[test]
    fn test_rpm_source_name_noop_for_clean_release() {
        let raw = "1.0.0";
        let safe = rpm_version_field(raw); // 1.0.0
        assert_eq!(safe, raw, "clean version must not change");
        assert_eq!(
            rpm_source_name("foo-1.0.0-source.tar.gz", raw, &safe),
            "foo-1.0.0-source.tar.gz"
        );
    }

    /// Contract no-op: when the version was sanitized but the source name does
    /// NOT embed the raw version, the rewrite leaves the name unchanged. Such
    /// a name is fine for a spec referencing `{{ Source }}` (set to this same
    /// name); a `%{version}`-templated `Source0:` is the user's responsibility
    /// to align via `{{ Source }}` instead.
    #[test]
    fn test_rpm_source_name_unchanged_when_version_not_embedded() {
        assert_eq!(
            rpm_source_name("foo-bar-source.tar.gz", "0.5.0-rc.1", "0.5.0~rc.1"),
            "foo-bar-source.tar.gz"
        );
    }

    /// Only the FIRST version-token occurrence is rewritten (`replacen`),
    /// mirroring rpmbuild's single `%{version}` expansion. A coincidental
    /// later recurrence of the raw version string is left intact.
    #[test]
    fn test_rpm_source_name_rewrites_only_first_occurrence() {
        // Version token appears twice; only the leading one is the real
        // `%{version}` slot, so the trailing recurrence must survive.
        assert_eq!(
            rpm_source_name(
                "0.5.0-rc.1-tool-0.5.0-rc.1-source.tar.gz",
                "0.5.0-rc.1",
                "0.5.0~rc.1"
            ),
            "0.5.0~rc.1-tool-0.5.0-rc.1-source.tar.gz"
        );
    }

    /// Reproduce the stage's scoped `Version`-override sequence for a
    /// user-supplied spec template (`spec_file:` path) and assert both halves
    /// of the invariant: (1) the rendered `Version:` field is RPM-safe
    /// (`-`-free) for a prerelease version, and (2) the global `Version`
    /// template var is RESTORED to the raw value afterward, so downstream
    /// stages (announce/publish run after srpm) see the real version.
    #[test]
    fn test_user_spec_render_is_rpm_safe_and_restores_version_var() {
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        let version = "0.5.0-rc.1".to_string();
        ctx.template_vars_mut().set("Version", &version);
        let rpm_safe_version = rpm_version_field(&version);

        // Exact sequence the stage runs around the user-spec render.
        let template = "Version: {{ Version }}";
        ctx.template_vars_mut().set("Version", &rpm_safe_version);
        let rendered = ctx.render_template(template).expect("render user spec");
        ctx.template_vars_mut().set("Version", &version);

        assert_eq!(rendered, "Version: 0.5.0~rc.1");
        assert!(!rendered.contains('-'), "rendered spec must be `-`-free");
        assert_eq!(
            ctx.template_vars().get("Version").map(String::as_str),
            Some("0.5.0-rc.1"),
            "global Version var must be restored to the raw value after render"
        );
    }

    /// Same non-leak invariant for the output-filename render path: the
    /// filename embeds the RPM-safe version, and the global `Version` var is
    /// restored to the raw value afterward.
    #[test]
    fn test_filename_render_is_rpm_safe_and_restores_version_var() {
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        let version = "0.5.0-rc.1".to_string();
        ctx.template_vars_mut().set("Version", &version);
        ctx.template_vars_mut().set("PackageName", "myapp");
        let rpm_safe_version = rpm_version_field(&version);

        let file_name_template = "{{ PackageName }}-{{ Version }}.src.rpm";
        ctx.template_vars_mut().set("Version", &rpm_safe_version);
        let rendered = ctx
            .render_template(file_name_template)
            .expect("render filename");
        ctx.template_vars_mut().set("Version", &version);

        assert_eq!(rendered, "myapp-0.5.0~rc.1.src.rpm");
        assert!(
            !rendered.contains("0.5.0-"),
            "filename must be `-`-free in version"
        );
        assert_eq!(
            ctx.template_vars().get("Version").map(String::as_str),
            Some("0.5.0-rc.1"),
            "global Version var must be restored to the raw value after render"
        );
    }

    /// An explicit `bins:` override is emitted verbatim into the `%files`
    /// section (binary→install-path map), in
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
            "",
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
            "",
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
            "",
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
    /// contents, license_file_name. Each emits the rpm-idiom shape
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
            "",
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
            "",
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
