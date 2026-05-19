//! `NfpmStage` — `Stage` implementation that drives `nfpm pkg` per crate / format.
//!
//! Step 1 (serial, `&mut ctx`) renders all templates and writes the YAML into
//! `_tmp_dir`; Step 2 (parallel) runs `nfpm pkg --packager <format>`.

use std::collections::HashMap;
use std::fs;
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::NfpmScripts;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

use crate::command::{is_arch_supported_for_format, nfpm_command, validate_format};
use crate::filename;
use crate::generate::{NfpmLibraryPaths, generate_nfpm_yaml_with_env};

pub struct NfpmStage;

/// A fully-staged nfpm job: config YAML written, filename decided,
/// subprocess args composed. Step 1 (serial, `&mut ctx`) renders all
/// templates and writes the YAML into `_tmp_dir`; Step 2 (parallel)
/// runs `nfpm pkg --packager <format>`. `_tmp_dir` keeps the config
/// file alive until the worker thread finishes.
struct NfpmJob {
    _tmp_dir: tempfile::TempDir,
    pkg_path: std::path::PathBuf,
    format: String,
    cmd_args: Vec<String>,
    /// Pre-parsed mtime for reproducible-build mtime stamping, or None
    /// when the config leaves `mtime` unset.
    mtime: Option<std::time::SystemTime>,
    mtime_repr: Option<String>,
    target: Option<String>,
    crate_name: String,
    pkg_metadata: std::collections::HashMap<String, String>,
}

impl Stage for NfpmStage {
    fn name(&self) -> &str {
        "nfpm"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("nfpm");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();
        let parallelism = ctx.options.parallelism.max(1);

        // Collect crates that have nfpm config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.nfpms.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        // Resolve version from template vars
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());

        // when the global skip_sign is active, zero out
        // all nFPM signature configuration in the generated YAML.
        let skip_sign = ctx.should_skip("sign");

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let mut jobs: Vec<NfpmJob> = Vec::new();

        // Validate nfpm config ID uniqueness across all crates (GoReleaser parity)
        {
            let mut seen_ids = std::collections::HashSet::new();
            for krate in &crates {
                if let Some(ref nfpm_configs) = krate.nfpms {
                    for cfg in nfpm_configs {
                        let id = cfg.id.as_deref().unwrap_or("default");
                        if !seen_ids.insert(id.to_string()) {
                            bail!(
                                "nfpm: duplicate config ID '{}' (each nfpm config must have a unique ID)",
                                id
                            );
                        }
                    }
                }
            }
        }

        for krate in &crates {
            let Some(nfpm_configs) = krate.nfpms.as_ref() else {
                continue;
            };

            // Collect all nfpm-eligible artifacts for this crate.
            // ByTypes(Binary, Header, CArchive, CShared)
            // filtered by ByGooses("linux", "ios", "android", "aix").
            let nfpm_artifact_kinds = &[
                ArtifactKind::Binary,
                ArtifactKind::Header,
                ArtifactKind::CArchive,
                ArtifactKind::CShared,
            ];
            let linux_binaries: Vec<_> = ctx
                .artifacts
                .by_kinds_and_crate(nfpm_artifact_kinds, &krate.name)
                .into_iter()
                .filter(|b| {
                    b.target
                        .as_deref()
                        .map(anodizer_core::target::is_nfpm_target)
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            for nfpm_cfg in nfpm_configs {
                let nfpm_id_for_log = nfpm_cfg.id.as_deref().unwrap_or("default").to_string();

                // GoReleaser Pro `nfpm.if`: template-conditional skip.
                // Rendered "false"/empty => skip with info log; render error => hard bail.
                // Hard-error on render failure intentionally diverges from stage-sign's
                // silent-skip-on-render-error (that is tracked as W1 in pro-features-audit.md
                // and must be fixed there too). A render failure means the user's template
                // references an unknown var; silently skipping would ship a release without
                // the packages the user asked for.
                if let Some(ref condition) = nfpm_cfg.if_condition {
                    let rendered = ctx.render_template(condition).with_context(|| {
                        format!(
                            "nfpm config '{}': `if` template render failed (expression: {})",
                            nfpm_id_for_log, condition
                        )
                    })?;
                    let trimmed = rendered.trim();
                    if trimmed.is_empty() || trimmed == "false" {
                        let reason = format!("if condition evaluated to '{}'", trimmed);
                        log.verbose(&format!(
                            "skipping nfpm config '{}': {}",
                            nfpm_id_for_log, reason
                        ));
                        ctx.remember_skip("nfpm", &nfpm_id_for_log, &reason);
                        continue;
                    }
                }

                // warn and skip when no output formats configured
                if nfpm_cfg.formats.is_empty() {
                    let nfpm_id = nfpm_id_for_log.as_str();
                    ctx.strict_guard(
                        &log,
                        &format!(
                            "nfpm config '{}': no output formats configured, skipping",
                            nfpm_id
                        ),
                    )?;
                    continue;
                }

                // warn when maintainer is empty (required for deb)
                let maintainer = nfpm_cfg.maintainer.as_deref().unwrap_or("");
                if maintainer.is_empty() {
                    let nfpm_id = nfpm_cfg.id.as_deref().unwrap_or("default");
                    log.warn(&format!(
                        "nfpm config '{}': maintainer is empty (required for deb packages)",
                        nfpm_id
                    ));
                }

                let is_meta = nfpm_cfg.meta == Some(true);

                // GoReleaser groups all artifacts by platform and creates ONE
                // package per platform containing ALL artifacts for that platform.
                // The tuple contains: (target, binary_paths, library_paths).
                let platform_groups: Vec<(Option<String>, Vec<String>, NfpmLibraryPaths)> =
                    if is_meta {
                        // Meta packages have no binary contents — use a synthetic entry
                        // so the loop below runs once per target (or once with no target).
                        if linux_binaries.is_empty() {
                            vec![(None, Vec::new(), NfpmLibraryPaths::default())]
                        } else {
                            let mut seen = std::collections::HashSet::new();
                            linux_binaries
                                .iter()
                                .filter(|b| {
                                    let key = b.target.clone().unwrap_or_default();
                                    seen.insert(key)
                                })
                                .map(|b| {
                                    (b.target.clone(), Vec::new(), NfpmLibraryPaths::default())
                                })
                                .collect()
                        }
                    } else {
                        // Apply ids filter: when the nfpm config specifies `ids`,
                        // only include artifacts whose metadata "id" is in the list.
                        let id_filtered: Vec<_> = if let Some(ref ids) = nfpm_cfg.ids {
                            linux_binaries
                                .iter()
                                .filter(|b| {
                                    b.metadata
                                        .get("id")
                                        .map(|bid| ids.contains(bid))
                                        .unwrap_or(false)
                                })
                                .collect()
                        } else {
                            linux_binaries.iter().collect()
                        };

                        // M8 — `goamd64: []string` filter (GR `nfpm.go:147`,
                        // calls `artifact.ByGoamd64s(fpm.GoAmd64...)`). When
                        // the config sets one or more variants, only `amd64`
                        // artifacts whose `amd64_variant` is in the list pass;
                        // non-amd64 artifacts are unaffected. Unset
                        // `amd64_variant` metadata is treated as `v1`. Empty
                        // `Vec` (`goamd64: []`) is a no-op (matches GR's
                        // `autoOr` zero-arg shape).
                        let filtered: Vec<_> = if let Some(ref wants) = nfpm_cfg.goamd64
                            && !wants.is_empty()
                        {
                            id_filtered
                                .into_iter()
                                .filter(|b| {
                                    let target = b.target.as_deref().unwrap_or("");
                                    let (_, arch) = anodizer_core::target::map_target(target);
                                    if arch != "amd64" {
                                        return true;
                                    }
                                    let v = b
                                        .metadata
                                        .get("amd64_variant")
                                        .map(String::as_str)
                                        .unwrap_or("v1");
                                    wants.iter().any(|w| w == v)
                                })
                                .collect()
                        } else {
                            id_filtered
                        };

                        // If the ids filter matched nothing but there ARE artifacts,
                        // warn and skip — the user likely misconfigured ids.
                        if filtered.is_empty() && !linux_binaries.is_empty() {
                            let nfpm_id = nfpm_cfg.id.as_deref().unwrap_or("default");
                            log.warn(&format!(
                                "nfpm config '{}': ids filter matched no binaries, skipping",
                                nfpm_id
                            ));
                            continue;
                        }

                        // If no artifacts found at all, use a single synthetic
                        // entry with a default path.
                        if filtered.is_empty() {
                            vec![(
                                None,
                                vec![format!("dist/{}", krate.name)],
                                NfpmLibraryPaths::default(),
                            )]
                        } else {
                            // Group by target: all artifacts for the same platform
                            // go into one package (GoReleaser parity).
                            // Split Binary artifacts from C library artifacts.
                            struct PlatformArtifacts {
                                binaries: Vec<String>,
                                libs: NfpmLibraryPaths,
                            }
                            let mut groups: std::collections::BTreeMap<
                                Option<String>,
                                PlatformArtifacts,
                            > = std::collections::BTreeMap::new();
                            for b in &filtered {
                                let entry = groups.entry(b.target.clone()).or_insert_with(|| {
                                    PlatformArtifacts {
                                        binaries: Vec::new(),
                                        libs: NfpmLibraryPaths::default(),
                                    }
                                });
                                let path = b.path.to_string_lossy().into_owned();
                                match b.kind {
                                    ArtifactKind::Header => entry.libs.headers.push(path),
                                    ArtifactKind::CArchive => entry.libs.c_archives.push(path),
                                    ArtifactKind::CShared => entry.libs.c_shared.push(path),
                                    _ => entry.binaries.push(path),
                                }
                            }
                            groups
                                .into_iter()
                                .map(|(t, pa)| (t, pa.binaries, pa.libs))
                                .collect()
                        }
                    };

                for (target, binary_paths, lib_paths) in &platform_groups {
                    // Derive Os/Arch from the target triple for template rendering
                    let (base_os, base_arch) = target
                        .as_deref()
                        .map(anodizer_core::target::map_target)
                        .unwrap_or_else(|| ("linux".to_string(), "amd64".to_string()));

                    for format in &nfpm_cfg.formats {
                        validate_format(format)
                            .with_context(|| format!("nfpm config for crate {}", krate.name))?;

                        // platform-format
                        // restrictions for iOS and AIX.
                        let (os, arch) = match base_os.as_str() {
                            "ios" => {
                                if format == "deb" {
                                    ("iphoneos-arm64".to_string(), base_arch.clone())
                                } else {
                                    log.status(&format!(
                                        "skipping ios for format '{}': only deb is supported",
                                        format
                                    ));
                                    continue;
                                }
                            }
                            "aix" => {
                                if base_arch != "ppc64" {
                                    log.status(&format!(
                                        "skipping aix/{}: only ppc64 is supported",
                                        base_arch
                                    ));
                                    continue;
                                }
                                if format == "rpm" {
                                    ("aix7.2".to_string(), "ppc".to_string())
                                } else {
                                    log.status(&format!(
                                        "skipping aix for format '{}': only rpm is supported",
                                        format
                                    ));
                                    continue;
                                }
                            }
                            _ => (base_os.clone(), base_arch.clone()),
                        };

                        // Validate architecture compatibility per format
                        if let Some(triple) = target.as_deref()
                            && !is_arch_supported_for_format(triple, format)
                        {
                            ctx.strict_guard(
                                &log,
                                &format!(
                                    "nfpm: skipping format '{}' for target '{}': architecture not supported",
                                    format, triple
                                ),
                            )?;
                            continue;
                        }

                        // Template-render key string fields before generating YAML.
                        // Errors are propagated (not silently swallowed) to match GoReleaser.
                        //
                        // GoReleaser Pro parity: fall back to project-level `metadata.*` when
                        // the nfpm config's own field is unset. Before this, `metadata.homepage`
                        // / `license` / `description` / `maintainers` were collected but silently
                        // unused (config-must-wire).
                        let mut rendered_cfg = nfpm_cfg.clone();
                        if rendered_cfg.description.is_none() {
                            rendered_cfg.description =
                                ctx.config.meta_description().map(str::to_string);
                        }
                        if rendered_cfg.maintainer.is_none() {
                            rendered_cfg.maintainer =
                                ctx.config.meta_first_maintainer().map(str::to_string);
                        }
                        if rendered_cfg.homepage.is_none() {
                            rendered_cfg.homepage = ctx.config.meta_homepage().map(str::to_string);
                        }
                        if rendered_cfg.license.is_none() {
                            rendered_cfg.license = ctx.config.meta_license().map(str::to_string);
                        }
                        if let Some(ref s) = rendered_cfg.description {
                            rendered_cfg.description = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.maintainer {
                            rendered_cfg.maintainer = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.homepage {
                            rendered_cfg.homepage = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.license {
                            rendered_cfg.license = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.vendor {
                            rendered_cfg.vendor = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.section {
                            rendered_cfg.section = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.priority {
                            rendered_cfg.priority = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.changelog {
                            rendered_cfg.changelog = Some(ctx.render_template(s)?);
                        }
                        // Template-render bindir and mtime (GoReleaser parity)
                        if let Some(ref s) = rendered_cfg.bindir {
                            rendered_cfg.bindir = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.mtime {
                            rendered_cfg.mtime = Some(ctx.render_template(s)?);
                        }
                        // Template-render script paths
                        if let Some(ref mut scripts) = rendered_cfg.scripts {
                            if let Some(ref s) = scripts.preinstall {
                                scripts.preinstall = Some(ctx.render_template(s)?);
                            }
                            if let Some(ref s) = scripts.postinstall {
                                scripts.postinstall = Some(ctx.render_template(s)?);
                            }
                            if let Some(ref s) = scripts.preremove {
                                scripts.preremove = Some(ctx.render_template(s)?);
                            }
                            if let Some(ref s) = scripts.postremove {
                                scripts.postremove = Some(ctx.render_template(s)?);
                            }
                        }
                        // Template-render signature key_file and key_name
                        if let Some(ref mut deb) = rendered_cfg.deb
                            && let Some(ref mut sig) = deb.signature
                            && let Some(ref s) = sig.key_file
                        {
                            sig.key_file = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref mut rpm) = rendered_cfg.rpm
                            && let Some(ref mut sig) = rpm.signature
                            && let Some(ref s) = sig.key_file
                        {
                            sig.key_file = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref mut apk) = rendered_cfg.apk
                            && let Some(ref mut sig) = apk.signature
                        {
                            if let Some(ref s) = sig.key_file {
                                sig.key_file = Some(ctx.render_template(s)?);
                            }
                            if let Some(ref s) = sig.key_name {
                                sig.key_name = Some(ctx.render_template(s)?);
                            }
                        }
                        // Template-render libdirs
                        if let Some(ref mut libdirs) = rendered_cfg.libdirs {
                            if let Some(ref s) = libdirs.header {
                                libdirs.header = Some(ctx.render_template(s)?);
                            }
                            if let Some(ref s) = libdirs.cshared {
                                libdirs.cshared = Some(ctx.render_template(s)?);
                            }
                            if let Some(ref s) = libdirs.carchive {
                                libdirs.carchive = Some(ctx.render_template(s)?);
                            }
                        }

                        // Template-render contents: src, dst, file_info.owner/group/mtime
                        if let Some(ref mut entries) = rendered_cfg.contents {
                            for entry in entries.iter_mut() {
                                entry.src = ctx.render_template(&entry.src)?;
                                entry.dst = ctx.render_template(&entry.dst)?;
                                if let Some(ref mut fi) = entry.file_info {
                                    if let Some(ref s) = fi.owner {
                                        fi.owner = Some(ctx.render_template(s)?);
                                    }
                                    if let Some(ref s) = fi.group {
                                        fi.group = Some(ctx.render_template(s)?);
                                    }
                                    if let Some(ref s) = fi.mtime {
                                        fi.mtime = Some(ctx.render_template(s)?);
                                    }
                                }
                            }
                        }

                        // GoReleaser Pro `templated_contents`: for each entry, read `src`,
                        // render its body through Tera, write to a temp file under
                        // `dist/nfpm-tmp/<crate>/<nfpm_id>/`, and append to `contents` using
                        // the temp file as the real source. User-supplied `dst` + `file_info`
                        // are preserved; only `src` is rewritten to the rendered temp path.
                        if let Some(templated_entries) = rendered_cfg.templated_contents.take()
                            && !templated_entries.is_empty()
                        {
                            {
                                let nfpm_id = nfpm_cfg.id.as_deref().unwrap_or("default");
                                let tmpl_dir =
                                    dist.join("nfpm-tmp").join(&krate.name).join(nfpm_id);
                                if !dry_run {
                                    fs::create_dir_all(&tmpl_dir).with_context(|| {
                                        format!(
                                            "nfpm: create templated-contents dir: {}",
                                            tmpl_dir.display()
                                        )
                                    })?;
                                }
                                let rendered_contents =
                                    rendered_cfg.contents.get_or_insert_with(Vec::new);
                                for (idx, mut entry) in templated_entries.into_iter().enumerate() {
                                    entry.src = ctx.render_template(&entry.src)?;
                                    entry.dst = ctx.render_template(&entry.dst)?;
                                    let body =
                                        fs::read_to_string(&entry.src).with_context(|| {
                                            format!(
                                                "nfpm: read templated_contents src: {}",
                                                entry.src
                                            )
                                        })?;
                                    let rendered_body =
                                        ctx.render_template(&body).with_context(|| {
                                            format!(
                                                "nfpm: render templated_contents body for {}",
                                                entry.src
                                            )
                                        })?;
                                    let base = std::path::Path::new(&entry.src)
                                        .file_name()
                                        .map(|s| s.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| format!("tmpl-{idx}"));
                                    let out_path = tmpl_dir.join(format!("{idx:03}-{base}"));
                                    if !dry_run {
                                        fs::write(&out_path, rendered_body.as_bytes())
                                            .with_context(|| {
                                                format!(
                                                    "nfpm: write rendered templated_contents: {}",
                                                    out_path.display()
                                                )
                                            })?;
                                    }
                                    entry.src = out_path.to_string_lossy().into_owned();
                                    rendered_contents.push(entry);
                                }
                            }
                        }

                        // GoReleaser Pro `templated_scripts`: same idea for lifecycle scripts.
                        // Each set field names a script file whose contents we render, write
                        // to a temp path, and substitute into `rendered_cfg.scripts`. Templated
                        // version wins over a same-named plain `scripts` entry.
                        if let Some(templated_scripts) = rendered_cfg.templated_scripts.take() {
                            let any = templated_scripts.preinstall.is_some()
                                || templated_scripts.postinstall.is_some()
                                || templated_scripts.preremove.is_some()
                                || templated_scripts.postremove.is_some();
                            if any {
                                let nfpm_id = nfpm_cfg.id.as_deref().unwrap_or("default");
                                let tmpl_dir =
                                    dist.join("nfpm-tmp").join(&krate.name).join(nfpm_id);
                                if !dry_run {
                                    fs::create_dir_all(&tmpl_dir).with_context(|| {
                                        format!(
                                            "nfpm: create templated-scripts dir: {}",
                                            tmpl_dir.display()
                                        )
                                    })?;
                                }
                                let scripts_out = rendered_cfg
                                    .scripts
                                    .get_or_insert_with(NfpmScripts::default);
                                let render_and_write =
                                    |name: &str,
                                     src_path: &str,
                                     ctx: &mut Context|
                                     -> Result<String> {
                                        let rendered_src = ctx.render_template(src_path)?;
                                        let body = fs::read_to_string(&rendered_src).with_context(
                                            || {
                                                format!(
                                                    "nfpm: read templated_script {}: {}",
                                                    name, rendered_src
                                                )
                                            },
                                        )?;
                                        let rendered_body =
                                            ctx.render_template(&body).with_context(|| {
                                                format!(
                                                    "nfpm: render templated_script {}: {}",
                                                    name, rendered_src
                                                )
                                            })?;
                                        let out_path = tmpl_dir.join(format!("script-{}", name));
                                        if !dry_run {
                                            fs::write(&out_path, rendered_body.as_bytes())
                                                .with_context(|| {
                                                    format!(
                                                        "nfpm: write rendered templated_script: {}",
                                                        out_path.display()
                                                    )
                                                })?;
                                        }
                                        Ok(out_path.to_string_lossy().into_owned())
                                    };
                                if let Some(ref s) = templated_scripts.preinstall {
                                    scripts_out.preinstall =
                                        Some(render_and_write("preinstall", s, ctx)?);
                                }
                                if let Some(ref s) = templated_scripts.postinstall {
                                    scripts_out.postinstall =
                                        Some(render_and_write("postinstall", s, ctx)?);
                                }
                                if let Some(ref s) = templated_scripts.preremove {
                                    scripts_out.preremove =
                                        Some(render_and_write("preremove", s, ctx)?);
                                }
                                if let Some(ref s) = templated_scripts.postremove {
                                    scripts_out.postremove =
                                        Some(render_and_write("postremove", s, ctx)?);
                                }
                            }
                        }

                        // Fill deb.arch_variant from artifact amd64 microarch
                        // when unset; explicit user config wins.
                        if let Some(ref mut deb) = rendered_cfg.deb
                            && deb.arch_variant.is_none()
                            && let Some(t) = target.as_deref()
                        {
                            let variant = linux_binaries
                                .iter()
                                .find(|b| b.target.as_deref() == Some(t))
                                .and_then(|b| b.metadata.get("amd64_variant").cloned());
                            deb.arch_variant = variant;
                        }

                        // Determine package file name (template or default).
                        // GoReleaser nfpm.go:68-70 — default is ProjectName
                        // (not the crate/binary name). Fall back to crate name
                        // only if project_name is empty.
                        //
                        // Computed BEFORE yaml generation because the lintian
                        // emission below needs `pkg_name` for both the file
                        // path and the destination key.
                        let pkg_name_owned: String =
                            if let Some(n) = nfpm_cfg.package_name.as_deref() {
                                n.to_string()
                            } else if !ctx.config.project_name.is_empty() {
                                ctx.config.project_name.clone()
                            } else {
                                krate.name.clone()
                            };
                        let pkg_name: &str = pkg_name_owned.as_str();
                        let ext = format_extension(format);

                        // M5: setupLintian — see `setup_lintian_overrides`
                        // for full rationale. Emits the lintian override
                        // file and injects the content entry, then clears
                        // the now-orphaned `lintian_overrides:` field on
                        // the rendered_cfg clone so the generated nfpm.yaml
                        // does not carry the dead key into nfpm input.
                        setup_lintian_overrides(
                            &mut rendered_cfg,
                            format,
                            pkg_name,
                            &arch,
                            &dist,
                            dry_run,
                        )?;

                        // Generate YAML per format so format-specific deps are selected.
                        // Pass the anodizer ctx env map so passphrase lookups
                        // see project `env:` / `env_files:` values (W6 fix).
                        let yaml_content = generate_nfpm_yaml_with_env(
                            &rendered_cfg,
                            &version,
                            binary_paths,
                            Some(format),
                            skip_sign,
                            lib_paths,
                            ctx.template_vars().all_env(),
                        )?;

                        // Ensure output directory exists
                        let output_dir = dist.join("linux");
                        if !dry_run {
                            fs::create_dir_all(&output_dir).with_context(|| {
                                format!("create nfpm output dir: {}", output_dir.display())
                            })?;
                        }

                        // Set nfpm-specific template vars (Os, Arch, Format,
                        // PackageName, ConventionalExtension, ConventionalFileName,
                        // Release, Epoch) before rendering file_name_template.
                        ctx.template_vars_mut().set("Os", &os);
                        ctx.template_vars_mut().set("Arch", &arch);
                        ctx.template_vars_mut()
                            .set("Target", target.as_deref().unwrap_or(""));
                        ctx.template_vars_mut().set("Format", format);
                        ctx.template_vars_mut().set("PackageName", pkg_name);
                        ctx.template_vars_mut().set("ConventionalExtension", ext);
                        // Per-packager ConventionalFileName (nfpm v2.44 parity):
                        // deb / rpm / apk / archlinux / ipk each have
                        // distinct filename conventions and arch
                        // translations. Falls back to the hand-rolled
                        // default for formats we don't recognise.
                        let fn_info = filename::FileNameInfo::from_config(
                            nfpm_cfg, pkg_name, &version, &arch, format,
                        );
                        let conventional = filename::conventional_filename(format, &fn_info)
                            .unwrap_or_else(|| format!("{pkg_name}_{version}_{os}_{arch}{ext}"));
                        ctx.template_vars_mut()
                            .set("ConventionalFileName", &conventional);
                        ctx.template_vars_mut()
                            .set("Release", nfpm_cfg.release.as_deref().unwrap_or(""));
                        ctx.template_vars_mut()
                            .set("Epoch", nfpm_cfg.epoch.as_deref().unwrap_or(""));

                        let pkg_filename = if let Some(tmpl) = &nfpm_cfg.file_name_template {
                            let rendered = ctx.render_template(tmpl).with_context(|| {
                                format!(
                                    "nfpm: render file_name_template for crate {} target {:?}",
                                    krate.name, target
                                )
                            })?;
                            // If the rendered template already ends with the
                            // format extension (e.g. the user used
                            // ConventionalExtension or ConventionalFileName),
                            // don't double-append it.
                            if !ext.is_empty() && rendered.ends_with(ext) {
                                rendered
                            } else {
                                format!("{rendered}{ext}")
                            }
                        } else {
                            format!("{pkg_name}_{version}_{os}_{arch}{ext}")
                        };
                        let pkg_path = output_dir.join(&pkg_filename);

                        // Build metadata: always include format, optionally include nfpm id
                        let mut pkg_metadata =
                            HashMap::from([("format".to_string(), format.clone())]);
                        if let Some(ref id) = nfpm_cfg.id {
                            pkg_metadata.insert("id".to_string(), id.clone());
                        }

                        if dry_run {
                            log.status(&format!(
                                "(dry-run) would run: nfpm pkg --packager {format} for crate {} target {:?}",
                                krate.name, target
                            ));
                            new_artifacts.push(Artifact {
                                kind: ArtifactKind::LinuxPackage,
                                name: String::new(),
                                path: pkg_path,
                                target: target.clone(),
                                crate_name: krate.name.clone(),
                                metadata: pkg_metadata,
                                size: None,
                            });
                            continue;
                        }

                        // Write temp nfpm YAML config
                        let tmp_dir =
                            tempfile::tempdir().context("create temp dir for nfpm config")?;
                        let config_path = tmp_dir.path().join("nfpm.yaml");
                        fs::write(&config_path, &yaml_content).with_context(|| {
                            format!("write nfpm config to {}", config_path.display())
                        })?;

                        // Pass the full file path (not directory) to nfpm
                        // --target so the output lands at the exact path we
                        // registered as the artifact.  This avoids mismatches
                        // between our predicted filename and nfpm's own naming.
                        let cmd_args = nfpm_command(
                            &config_path.to_string_lossy(),
                            format,
                            &pkg_path.to_string_lossy(),
                        );

                        // Render mtime once in Step 1 so Step 2 doesn't touch
                        // ctx; pre-parse into SystemTime so workers can call
                        // set_file_mtime directly.
                        let (mtime, mtime_repr) = if let Some(ref raw_mtime) = nfpm_cfg.mtime {
                            let rendered_mtime = ctx
                                .render_template(raw_mtime)
                                .unwrap_or_else(|_| raw_mtime.clone());
                            match anodizer_core::util::parse_mod_timestamp(&rendered_mtime) {
                                Ok(mt) => (Some(mt), Some(rendered_mtime)),
                                Err(e) => {
                                    log.warn(&format!(
                                        "nfpm: invalid mtime '{rendered_mtime}': {e}"
                                    ));
                                    (None, None)
                                }
                            }
                        } else {
                            (None, None)
                        };

                        jobs.push(NfpmJob {
                            _tmp_dir: tmp_dir,
                            pkg_path: pkg_path.clone(),
                            format: format.clone(),
                            cmd_args,
                            mtime,
                            mtime_repr,
                            target: target.clone(),
                            crate_name: krate.name.clone(),
                            pkg_metadata,
                        });
                    }
                }
            }
        }

        anodizer_core::template::clear_per_target_vars(ctx.template_vars_mut());
        // nfpm also uses its own per-format / per-packaging vars; clear
        // them here so user-template state doesn't leak into downstream
        // stages like announce or publish.
        for extra in [
            "Format",
            "PackageName",
            "ConventionalExtension",
            "ConventionalFileName",
            "Release",
            "Epoch",
        ] {
            ctx.template_vars_mut().set(extra, "");
        }

        // ----------------------------------------------------------------
        // Step 2 (parallel): run `nfpm pkg --packager <format>` per job.
        // Bounded concurrency via chunks(parallelism). Each worker returns
        // the populated Artifact; Step 3 registers them serially.
        // ----------------------------------------------------------------
        if !jobs.is_empty() {
            let run_job = |job: &NfpmJob| -> Result<Artifact> {
                let thread_log = anodizer_core::log::StageLogger::new("nfpm", log.verbosity());

                thread_log.status(&format!("running: {}", job.cmd_args.join(" ")));

                let output = Command::new(&job.cmd_args[0])
                    .args(&job.cmd_args[1..])
                    .output()
                    .with_context(|| {
                        format!(
                            "execute nfpm for format {} (crate {} target {:?})",
                            job.format, job.crate_name, job.target
                        )
                    })?;
                thread_log.check_output(output, "nfpm")?;

                // Reproducible-build mtime — pre-parsed in Step 1.
                if let Some(mt) = job.mtime {
                    if let Err(e) = anodizer_core::util::set_file_mtime(&job.pkg_path, mt) {
                        thread_log.warn(&format!(
                            "nfpm: failed to apply mtime to {}: {}",
                            job.pkg_path.display(),
                            e
                        ));
                    } else if let Some(ref repr) = job.mtime_repr {
                        thread_log.verbose(&format!(
                            "nfpm: applied mtime={repr} to {}",
                            job.pkg_path.display()
                        ));
                    }
                }

                Ok(Artifact {
                    kind: ArtifactKind::LinuxPackage,
                    name: String::new(),
                    path: job.pkg_path.clone(),
                    target: job.target.clone(),
                    crate_name: job.crate_name.clone(),
                    metadata: job.pkg_metadata.clone(),
                    size: None,
                })
            };

            let results =
                anodizer_core::parallel::run_parallel_chunks(&jobs, parallelism, "nfpm", run_job)?;
            new_artifacts.extend(results);
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

/// Return the file extension for a given nfpm packager format.
pub(crate) fn format_extension(format: &str) -> &str {
    match format {
        "deb" | "termux.deb" => ".deb",
        "rpm" => ".rpm",
        "apk" => ".apk",
        "archlinux" => ".pkg.tar.zst",
        "ipk" => ".ipk",
        _ => "",
    }
}

/// Emit a Debian `lintian` override file and inject the matching content
/// entry into the rendered nfpm config, then clear the now-orphaned
/// `lintian_overrides:` field so the YAML output stays clean.
///
/// M5: GoReleaser's `setupLintian` (`internal/pipe/nfpm/nfpm.go:601-623`)
/// writes a file to `<dist>/<format>/<package>_<arch>/lintian` whose body
/// is one `<package>: <override>` line per entry in `deb.lintian_overrides`,
/// then appends a `Content` mapping that path into the package at
/// `/usr/share/lintian/overrides/<package>` (mode 0644, packager-scoped to
/// `"deb"`). Anodizer previously parsed `deb.lintian_overrides` into a YAML
/// key but `nfpm` itself does not consume that key, so the override file
/// was silently dropped from the resulting `.deb` / `termux.deb`.
///
/// This helper performs the file emission and content injection in
/// lockstep with GR. When `dry_run` is true the on-disk write is skipped
/// (the content entry is still injected so the generated YAML reflects
/// what would ship). The helper is a no-op for non-deb formats and for
/// configs where `lintian_overrides` is unset / empty.
///
/// Returns an error only when the on-disk write fails — a configured
/// override list always reaches the `contents:` array.
pub(crate) fn setup_lintian_overrides(
    rendered_cfg: &mut anodizer_core::config::NfpmConfig,
    format: &str,
    pkg_name: &str,
    arch: &str,
    dist: &std::path::Path,
    dry_run: bool,
) -> Result<()> {
    if format != "deb" && format != "termux.deb" {
        return Ok(());
    }
    let Some(deb_cfg) = rendered_cfg.deb.as_mut() else {
        return Ok(());
    };
    let Some(overrides) = deb_cfg.lintian_overrides.take() else {
        return Ok(());
    };
    if overrides.is_empty() {
        return Ok(());
    }

    let pkg_dir = dist.join(format).join(format!("{pkg_name}_{arch}"));
    let lintian_path = pkg_dir.join("lintian");
    if !dry_run {
        fs::create_dir_all(&pkg_dir)
            .with_context(|| format!("nfpm lintian: create dir {}", pkg_dir.display()))?;
        let body: String = overrides
            .iter()
            .map(|ov| format!("{pkg_name}: {ov}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&lintian_path, body)
            .with_context(|| format!("nfpm lintian: write {}", lintian_path.display()))?;
    }

    let entry = anodizer_core::config::NfpmContent {
        src: lintian_path.to_string_lossy().into_owned(),
        dst: format!("/usr/share/lintian/overrides/{pkg_name}"),
        content_type: None,
        file_info: Some(anodizer_core::config::NfpmFileInfo {
            owner: None,
            group: None,
            mode: Some(anodizer_core::config::StringOrU32(0o644)),
            mtime: None,
        }),
        packager: Some("deb".to_string()),
        expand: None,
    };
    rendered_cfg
        .contents
        .get_or_insert_with(Vec::new)
        .push(entry);
    Ok(())
}
