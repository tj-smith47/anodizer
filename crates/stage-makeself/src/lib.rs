use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind, matches_id_filter};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

// ---------------------------------------------------------------------------
// LSM (Linux Software Map) metadata
// ---------------------------------------------------------------------------

struct Lsm {
    title: String,
    version: String,
    description: String,
    keywords: Vec<String>,
    maintained_by: String,
    primary_site: String,
    platform: String,
    copying_policy: String,
}

impl Lsm {
    fn render(&self) -> String {
        let mut sb = String::from("Begin4\n");
        let mut w = |name: &str, value: &str| {
            if !value.is_empty() {
                sb.push_str(&format!("{}: {}\n", name, value));
            }
        };
        w("Title", &self.title);
        w("Version", &self.version);
        w("Description", &self.description);
        if !self.keywords.is_empty() {
            w("Keywords", &self.keywords.join(", "));
        }
        w("Maintained-by", &self.maintained_by);
        w("Author", &self.maintained_by);
        w("Primary-site", &self.primary_site);
        w("Platforms", &self.platform);
        w("Copying-policy", &self.copying_policy);
        sb.push_str("End");
        sb
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build the makeself command arguments.
fn make_args(
    name: &str,
    filename: &str,
    compression: Option<&str>,
    script: &str,
    extra_args: &[String],
    packaging_date: Option<&str>,
) -> Vec<String> {
    let mut args = vec!["--quiet".to_string()];

    // Compression default is `--xz` (not makeself's `--gzip`): the gzip
    // pipeline embeds the wallclock mtime of the intermediate tarball into
    // the gzip stream header (gzip reads stdin from a regular file, sees its
    // mtime, ignores SOURCE_DATE_EPOCH), which drifts the compressed bytes
    // and the CRCsum/MD5/SHA values embedded in the .run shell header. xz
    // does not embed wall-clock data and is byte-stable under SDE. Users can
    // override via `compression: gzip` in config but should expect non-
    // deterministic .run output under the harness in that case.
    match compression {
        Some("gzip") => args.push("--gzip".to_string()),
        Some("bzip2") => args.push("--bzip2".to_string()),
        Some("xz") => args.push("--xz".to_string()),
        Some("lzo") => args.push("--lzo".to_string()),
        Some("compress") => args.push("--compress".to_string()),
        Some("none") => args.push("--nocomp".to_string()),
        None => args.push("--xz".to_string()),
        Some(_) => {} // unknown values pass through to makeself default
    }

    args.push("--lsm".to_string());
    args.push("package.lsm".to_string());

    // Pin the extraction-target dir name. Without `--target`, makeself falls
    // back to `makeself-$$-$(date +%Y%m%d%H%M%S)` which embeds the PID + the
    // wallclock and drifts byte-by-byte between two harness runs. The
    // filename (sans `.run` extension) is stable across runs by construction
    // (it's templated from config + version + target) and reads naturally
    // when the user extracts to inspect the archive.
    let target_dir = filename.strip_suffix(".run").unwrap_or(filename);
    args.push("--target".to_string());
    args.push(target_dir.to_string());

    // Pin the packaging-date header under SOURCE_DATE_EPOCH so the .run
    // header is byte-stable across runs. makeself's default
    // `DATE=`LC_ALL=C date`` reads wall-clock and otherwise leaks into the
    // .run file (the `Date of packaging:` line in the embedded header).
    if let Some(date) = packaging_date {
        args.push("--packaging-date".to_string());
        args.push(date.to_string());
    }

    args.extend(extra_args.iter().cloned());

    // positional args: archive_dir output_file label startup_script.
    // Output is the bare filename (relative to current_dir); using an
    // absolute path here would leak the per-run worktree prefix into
    // makeself's embedded `MS_COMMAND` variable and break determinism.
    args.push(".".to_string());
    args.push(filename.to_string());
    args.push(name.to_string());
    args.push(script.to_string());

    args
}

/// Format a packaging date string for makeself's `--packaging-date` flag.
/// Resolves from `SOURCE_DATE_EPOCH`; returns `None` when SDE is unset so
/// normal production runs keep makeself's default `LC_ALL=C date` behaviour.
fn resolve_packaging_date() -> Option<String> {
    // Format mirrors `LC_ALL=C date -u` output (which is what makeself's
    // default `DATE=`LC_ALL=C date`` produces in the harness's UTC=Etc/UTC
    // env), keeping the embedded `Date of packaging:` line readable.
    anodizer_core::sde::source_date_epoch()
        .map(|dt| dt.format("%a %b %e %H:%M:%S UTC %Y").to_string())
}

/// Recursively pin every regular file's mtime in `dir` to `epoch_secs`.
///
/// makeself wraps `tar` over the work directory; tar embeds each file's
/// on-disk mtime into the archive header. `fs::copy` stamps destination
/// files with the current wallclock, so two harness runs produce identical
/// file contents but with different mtimes — that drifts the tar payload,
/// the gzip compressed bytes (length + content), and ultimately the
/// `size_bytes` field that artifacts.json carries for the `.run` artifact.
/// Pinning every file's mtime to `SOURCE_DATE_EPOCH` removes the drift.
fn pin_workdir_mtimes(dir: &Path, epoch_secs: i64) -> Result<()> {
    let mut stack: Vec<std::path::PathBuf> = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        for entry in fs::read_dir(&p)
            .with_context(|| format!("makeself: read_dir {} for mtime pin", p.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                anodizer_core::util::set_file_mtime_epoch(&path, epoch_secs)
                    .with_context(|| format!("makeself: pin mtime on {}", path.display()))?;
            }
        }
    }
    Ok(())
}

/// Group artifacts by platform string (e.g. "linux_amd64").
///
/// `BTreeMap` (not `HashMap`) so iteration order is deterministic across
/// runs — callers iterate the result to register one makeself Artifact per
/// platform, and `HashMap` iteration order is randomised per process. The
/// matching `stage-archive` regression shipped per-run drift into
/// `dist/artifacts.json`; this stage uses the same pattern so it gets the
/// same fix preemptively.
fn group_by_platform(artifacts: &[Artifact]) -> BTreeMap<String, Vec<&Artifact>> {
    let mut groups: BTreeMap<String, Vec<&Artifact>> = BTreeMap::new();
    for a in artifacts {
        let platform = match &a.target {
            Some(t) => {
                let (os, arch) = anodizer_core::target::map_target(t);
                format!("{}_{}", os, arch)
            }
            None => "unknown".to_string(),
        };
        groups.entry(platform).or_default().push(a);
    }
    groups
}

// ---------------------------------------------------------------------------
// MakeselfStage
// ---------------------------------------------------------------------------

pub struct MakeselfStage;

/// A fully-prepared makeself job ready for parallel execution. Step 1
/// (serial, requires `&mut ctx` for template rendering) populates this;
/// Step 2 (parallel, `std::thread::scope`) consumes it — filesystem
/// preparation + `makeself` subprocess only. Struct carries only owned data
/// so worker threads never touch `ctx`.
struct MakeselfJob {
    id: String,
    filename: String,
    work_dir: std::path::PathBuf,
    output_path: std::path::PathBuf,
    rendered_name: String,
    script_src: std::path::PathBuf,
    script_basename: String,
    rendered_compression: Option<String>,
    extra_args: Vec<String>,
    lsm_text: String,
    /// (source_path, name_in_archive) pairs for each binary to copy in.
    binaries: Vec<(std::path::PathBuf, String)>,
    /// (source_path, destination_relative_to_work_dir) pairs for extra files.
    extra_files: Vec<(std::path::PathBuf, String)>,
    /// Fields needed to register the resulting artifact.
    primary_target: Option<String>,
    primary_crate_name: String,
    primary_replaces: Option<String>,
}

impl Stage for MakeselfStage {
    fn name(&self) -> &str {
        "makeself"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("makeself");
        let configs = ctx.config.makeselfs.clone();

        if configs.is_empty() {
            return Ok(());
        }

        let dist = ctx.config.dist.clone();
        let dry_run = ctx.options.dry_run;
        let parallelism = ctx.options.parallelism.max(1);

        // Validate IDs are unique
        let mut seen_ids = std::collections::HashSet::new();
        for cfg in &configs {
            let id = cfg.id.as_deref().unwrap_or("default");
            if !seen_ids.insert(id.to_string()) {
                anyhow::bail!("makeself: duplicate id '{}'", id);
            }
        }

        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());
        let project_name = ctx.config.project_name.clone();

        // ----------------------------------------------------------------
        // Step 1 (serial): render every template, collect MakeselfJob
        // structs containing fully-owned data ready for parallel exec.
        // ----------------------------------------------------------------
        let mut jobs: Vec<MakeselfJob> = Vec::new();

        for cfg in &configs {
            // Skip configs marked skip:
            if let Some(ref d) = cfg.skip {
                let off = d
                    .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .with_context(|| "makeself: render skip template")?;
                if off {
                    log.verbose("makeself config skipped");
                    continue;
                }
            }

            let id = cfg.id.as_deref().unwrap_or("default");
            let name = cfg.name.as_deref().unwrap_or(&project_name);
            // GoReleaser makeself.go:31 default name_template:
            //   {{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}
            //   {{ with .Arm }}v{{ . }}{{ end }}
            //   {{ with .Mips }}_{{ . }}{{ end }}
            //   {{ if not (eq .Amd64 "v1") }}{{ .Amd64 }}{{ end }}.run
            // Rendered here using the Tera-style syntax anodizer exposes.
            let default_name_template = concat!(
                "{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}",
                "{% if Arm %}v{{ Arm }}{% endif %}",
                "{% if Mips %}_{{ Mips }}{% endif %}",
                "{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}.run",
            );
            let name_template = cfg.filename.as_deref().unwrap_or(default_name_template);

            let script = cfg.script.as_deref().unwrap_or("");
            if script.is_empty() {
                anyhow::bail!("makeself: 'script' is required for config id '{}'", id);
            }

            // Default goos: linux and darwin
            let goos_filter: Vec<String> = cfg
                .goos
                .clone()
                .unwrap_or_else(|| vec!["linux".to_string(), "darwin".to_string()]);

            // Collect matching binary artifacts (cloned to release borrow on ctx)
            let all_binaries: Vec<Artifact> = ctx
                .artifacts
                .all()
                .iter()
                .filter(|a| {
                    matches!(
                        a.kind,
                        ArtifactKind::Binary
                            | ArtifactKind::UniversalBinary
                            | ArtifactKind::Header
                            | ArtifactKind::CArchive
                            | ArtifactKind::CShared
                    )
                })
                .filter(|a| matches_id_filter(a, cfg.ids.as_deref()))
                .filter(|a| {
                    // Filter by goos
                    if let Some(ref target) = a.target {
                        let (os, _) = anodizer_core::target::map_target(target);
                        goos_filter.iter().any(|g| g == &os)
                    } else {
                        false
                    }
                })
                .filter(|a| {
                    // Filter by goarch if configured
                    if let Some(ref goarch) = cfg.goarch {
                        if let Some(ref target) = a.target {
                            let (_, arch) = anodizer_core::target::map_target(target);
                            goarch.iter().any(|g| g == &arch)
                        } else {
                            false
                        }
                    } else {
                        true
                    }
                })
                .cloned()
                .collect();

            if all_binaries.is_empty() {
                anyhow::bail!(
                    "makeself: no binaries found for config '{}' with goos {:?}",
                    id,
                    goos_filter
                );
            }

            let groups = group_by_platform(&all_binaries);

            for (platform, binaries) in &groups {
                let primary = binaries[0];
                let (os, arch) = primary
                    .target
                    .as_deref()
                    .map(anodizer_core::target::map_target)
                    .unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()));

                // Render templates
                ctx.template_vars_mut().set("Os", &os);
                ctx.template_vars_mut().set("Arch", &arch);
                ctx.template_vars_mut()
                    .set("Target", primary.target.as_deref().unwrap_or(""));

                // Per-target variant vars (mirror stage-build/src/lib.rs 1530-1537)
                // so the default name_template can render v7/v8/v1/mips suffixes.
                let first_component = primary
                    .target
                    .as_deref()
                    .and_then(|t| t.split('-').next())
                    .unwrap_or("");
                // Clear previous values so each target starts clean.
                ctx.template_vars_mut().set("Arm", "");
                ctx.template_vars_mut().set("Arm64", "");
                ctx.template_vars_mut().set("Amd64", "");
                ctx.template_vars_mut().set("Mips", "");
                ctx.template_vars_mut().set("I386", "");
                match first_component {
                    "aarch64" => ctx.template_vars_mut().set("Arm64", "v8"),
                    "armv7" | "armv7l" => ctx.template_vars_mut().set("Arm", "7"),
                    "armv6" | "armv6l" | "arm" => ctx.template_vars_mut().set("Arm", "6"),
                    "x86_64" => ctx.template_vars_mut().set("Amd64", "v1"),
                    "i686" | "i386" | "i586" => ctx.template_vars_mut().set("I386", "sse2"),
                    c if c.starts_with("mips") => {
                        // Set Mips variant (mips, mipsel, mips64, mips64el)
                        ctx.template_vars_mut().set("Mips", c);
                    }
                    _ => {}
                }

                let rendered_name = if cfg.name.is_some() {
                    ctx.render_template(name)?
                } else {
                    project_name.clone()
                };

                let filename = if !name_template.is_empty() {
                    let rendered = ctx.render_template(name_template)?;
                    if rendered.ends_with(".run") {
                        rendered
                    } else {
                        format!("{}.run", rendered)
                    }
                } else {
                    // Include the per-arch variant suffix so multi-target ARM /
                    // MIPS / x86 builds for the same project don't collide on
                    // disk. Mirrors the suffix logic in the archive default
                    // template (`stage-archive/src/lib.rs`).
                    let arm = ctx.template_vars().get("Arm").cloned().unwrap_or_default();
                    let mips = ctx.template_vars().get("Mips").cloned().unwrap_or_default();
                    let amd64 = ctx
                        .template_vars()
                        .get("Amd64")
                        .cloned()
                        .unwrap_or_default();
                    let mut suffix = String::new();
                    if !arm.is_empty() {
                        // ARM v6/v7 → `_arm` already; append the variant.
                        suffix.push('v');
                        suffix.push_str(&arm);
                    }
                    if !mips.is_empty() {
                        suffix.push('_');
                        suffix.push_str(&mips);
                    }
                    if !amd64.is_empty() && amd64 != "v1" {
                        // Only append non-default amd64 variants (v2/v3/v4).
                        suffix.push_str(&amd64);
                    }
                    format!("{}_{}_{}_{}{}.run", project_name, version, os, arch, suffix)
                };

                let rendered_description = cfg
                    .description
                    .as_deref()
                    .map(|d| ctx.render_template(d))
                    .transpose()?
                    .unwrap_or_default();
                let rendered_maintainer = cfg
                    .maintainer
                    .as_deref()
                    .map(|m| ctx.render_template(m))
                    .transpose()?
                    .unwrap_or_default();
                let rendered_homepage = cfg
                    .homepage
                    .as_deref()
                    .map(|h| ctx.render_template(h))
                    .transpose()?
                    .unwrap_or_default();
                let rendered_license = cfg
                    .license
                    .as_deref()
                    .map(|l| ctx.render_template(l))
                    .transpose()?
                    .unwrap_or_default();
                let rendered_script = ctx.render_template(script)?;
                let rendered_compression = cfg
                    .compression
                    .as_deref()
                    .map(|c| ctx.render_template(c))
                    .transpose()?;

                let keywords: Vec<String> = cfg
                    .keywords
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(|k| ctx.render_template(k))
                    .collect::<Result<Vec<_>>>()?;

                let extra_args: Vec<String> = cfg
                    .extra_args
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(|a| ctx.render_template(a))
                    .collect::<Result<Vec<_>>>()?;

                // Build LSM metadata
                let lsm = Lsm {
                    title: rendered_name.clone(),
                    version: version.clone(),
                    description: rendered_description,
                    keywords,
                    maintained_by: rendered_maintainer,
                    primary_site: rendered_homepage,
                    platform: platform.clone(),
                    copying_policy: rendered_license,
                };

                // Set up working directory
                let work_dir = dist.join("makeself").join(id).join(platform);

                if dry_run {
                    log.status(&format!(
                        "(dry-run) would create makeself package: {}",
                        filename
                    ));
                    continue;
                }

                // Collect binary (src, name_in_archive) pairs so Step 2 can
                // copy them without borrowing ctx.artifacts.
                let job_binaries: Vec<(std::path::PathBuf, String)> = binaries
                    .iter()
                    .map(|b| (b.path.clone(), b.name.clone()))
                    .collect();

                // Resolve extra-file (src, dest-relative) pairs now — the
                // decision depends on strip_parent / destination which are
                // cheap to pre-compute.
                let job_extra_files: Vec<(std::path::PathBuf, String)> =
                    if let Some(ref files) = cfg.files {
                        files
                            .iter()
                            .map(|f| {
                                let src = Path::new(&f.source);
                                let dest_name = if let Some(ref dst) = f.destination {
                                    dst.clone()
                                } else if f.strip_parent.unwrap_or(false) {
                                    src.file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or(&f.source)
                                        .to_string()
                                } else {
                                    f.source.clone()
                                };
                                (src.to_path_buf(), dest_name)
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };

                let script_path = Path::new(&rendered_script).to_path_buf();
                let script_basename = script_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("setup.sh")
                    .to_string();

                let output_path = dist.join(&filename);
                let lsm_text = lsm.render();

                jobs.push(MakeselfJob {
                    id: id.to_string(),
                    filename: filename.clone(),
                    work_dir,
                    output_path,
                    rendered_name,
                    script_src: script_path,
                    script_basename,
                    rendered_compression,
                    extra_args,
                    lsm_text,
                    binaries: job_binaries,
                    extra_files: job_extra_files,
                    primary_target: primary.target.clone(),
                    primary_crate_name: primary.crate_name.clone(),
                    primary_replaces: primary.metadata.get("replaces").cloned(),
                });
            }
        }

        if jobs.is_empty() {
            return Ok(());
        }

        // ----------------------------------------------------------------
        // Step 2 (parallel): each job = one `makeself` subprocess invocation
        // with its own work dir. Bounded concurrency via chunks(parallelism).
        // Workers return the fully-populated `Artifact` so Step 3 can
        // register them serially in ctx.artifacts.
        // ----------------------------------------------------------------
        let run_job = |job: &MakeselfJob| -> Result<Artifact> {
            let thread_log = anodizer_core::log::StageLogger::new("makeself", log.verbosity());

            fs::create_dir_all(&job.work_dir)
                .with_context(|| format!("makeself: create dir {}", job.work_dir.display()))?;

            for (src, name) in &job.binaries {
                let dst = job.work_dir.join(name);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(src, &dst).with_context(|| {
                    format!(
                        "makeself: copy binary {} -> {}",
                        src.display(),
                        dst.display()
                    )
                })?;
            }

            for (src, dest_rel) in &job.extra_files {
                let dst = job.work_dir.join(dest_rel);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(src, &dst).with_context(|| {
                    format!("makeself: copy file {} -> {}", src.display(), dst.display())
                })?;
            }

            fs::copy(&job.script_src, job.work_dir.join(&job.script_basename))
                .with_context(|| format!("makeself: copy script {}", job.script_src.display()))?;

            fs::write(job.work_dir.join("package.lsm"), &job.lsm_text).with_context(|| {
                format!("makeself: write LSM file in {}", job.work_dir.display())
            })?;

            // Pin every file's mtime to SOURCE_DATE_EPOCH so tar embeds the
            // same per-file timestamps across runs. Without this, fs::copy
            // stamps the destination with the current wallclock and the
            // resulting tar.gz payload differs between consecutive runs.
            if let Some(epoch) = anodizer_core::sde::source_date_epoch().map(|dt| dt.timestamp()) {
                pin_workdir_mtimes(&job.work_dir, epoch)?;
            }

            let packaging_date = resolve_packaging_date();
            let args = make_args(
                &job.rendered_name,
                &job.filename,
                job.rendered_compression.as_deref(),
                &format!("./{}", job.script_basename),
                &job.extra_args,
                packaging_date.as_deref(),
            );

            thread_log.status(&format!("creating makeself package: {}", job.filename));

            let output = Command::new("makeself")
                .args(&args)
                .current_dir(&job.work_dir)
                .output()
                .with_context(|| {
                    format!(
                        "makeself: failed to spawn 'makeself {}' in {}",
                        args.join(" "),
                        job.work_dir.display()
                    )
                })?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                anyhow::bail!(
                    "makeself command failed for '{}' (id={}): {}{}",
                    job.filename,
                    job.id,
                    stdout,
                    stderr
                );
            }

            let built_path = job.work_dir.join(&job.filename);
            fs::rename(&built_path, &job.output_path)
                .or_else(|_| {
                    fs::copy(&built_path, &job.output_path)?;
                    fs::remove_file(&built_path)
                })
                .with_context(|| {
                    format!(
                        "makeself: move {} -> {}",
                        built_path.display(),
                        job.output_path.display()
                    )
                })?;

            let mut metadata = HashMap::new();
            metadata.insert("id".to_string(), job.id.clone());
            metadata.insert("format".to_string(), "makeself".to_string());
            if let Some(replaces) = &job.primary_replaces {
                metadata.insert("replaces".to_string(), replaces.clone());
            }

            Ok(Artifact {
                kind: ArtifactKind::Makeself,
                name: job.filename.clone(),
                path: job.output_path.clone(),
                target: job.primary_target.clone(),
                crate_name: job.primary_crate_name.clone(),
                metadata,
                size: None,
            })
        };

        let built_artifacts =
            anodizer_core::parallel::run_parallel_chunks(&jobs, parallelism, "makeself", run_job)?;

        // ----------------------------------------------------------------
        // Step 3 (serial): register artifacts in ctx. Serial because
        // ArtifactRegistry takes &mut self.
        // ----------------------------------------------------------------
        for artifact in built_artifacts {
            ctx.artifacts.add(artifact);
        }

        // Match flatpak/snapcraft/nfpm: clear per-target template vars so
        // downstream stages (announce, publish) don't render with a stale
        // `Os=linux` / `Arch=arm64` left over from the last packaging
        // iteration.
        anodizer_core::template::clear_per_target_vars(ctx.template_vars_mut());

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::MakeselfConfig;
    use std::path::PathBuf;

    #[test]
    fn test_lsm_render() {
        let lsm = Lsm {
            title: "MyApp".to_string(),
            version: "1.0.0".to_string(),
            description: "A test application".to_string(),
            keywords: vec!["test".to_string(), "app".to_string()],
            maintained_by: "Test User".to_string(),
            primary_site: "https://example.com".to_string(),
            platform: "linux_amd64".to_string(),
            copying_policy: "MIT".to_string(),
        };
        let rendered = lsm.render();
        assert!(rendered.starts_with("Begin4\n"));
        assert!(rendered.ends_with("End"));
        assert!(rendered.contains("Title: MyApp"));
        assert!(rendered.contains("Version: 1.0.0"));
        assert!(rendered.contains("Keywords: test, app"));
        assert!(rendered.contains("Copying-policy: MIT"));
    }

    #[test]
    fn test_make_args_default_compression() {
        let args = make_args("MyApp", "myapp.run", None, "./setup.sh", &[], None);
        assert_eq!(args[0], "--quiet");
        assert!(args.contains(&"--lsm".to_string()));
        assert!(args.contains(&"package.lsm".to_string()));
        assert!(args.contains(&".".to_string()));
        assert!(args.contains(&"myapp.run".to_string()));
        assert!(args.contains(&"MyApp".to_string()));
        assert!(args.contains(&"./setup.sh".to_string()));
        assert!(
            args.contains(&"--xz".to_string()),
            "compression must default to --xz, not makeself's --gzip default: {args:?}"
        );
    }

    #[test]
    fn test_make_args_target_pinned() {
        let args = make_args("MyApp", "myapp.run", None, "./setup.sh", &[], None);
        let idx = args
            .iter()
            .position(|a| a == "--target")
            .expect("expected --target in args");
        assert_eq!(
            args[idx + 1],
            "myapp",
            "target must derive from filename without .run extension"
        );
    }

    #[test]
    fn test_make_args_target_handles_filename_without_run_ext() {
        let args = make_args("MyApp", "weird-name", None, "./setup.sh", &[], None);
        let idx = args
            .iter()
            .position(|a| a == "--target")
            .expect("expected --target in args");
        assert_eq!(args[idx + 1], "weird-name");
    }

    #[test]
    fn test_make_args_xz_compression() {
        let args = make_args("MyApp", "myapp.run", Some("xz"), "./setup.sh", &[], None);
        assert!(args.contains(&"--xz".to_string()));
    }

    #[test]
    fn test_make_args_gzip_passthrough() {
        // Explicit gzip is honored even though it's non-deterministic — user's
        // choice respected; they can fix by setting compression: xz.
        let args = make_args("MyApp", "myapp.run", Some("gzip"), "./setup.sh", &[], None);
        assert!(args.contains(&"--gzip".to_string()));
    }

    #[test]
    fn test_make_args_no_compression() {
        let args = make_args("MyApp", "myapp.run", Some("none"), "./setup.sh", &[], None);
        assert!(args.contains(&"--nocomp".to_string()));
    }

    #[test]
    fn test_make_args_extra_args() {
        let extra = vec!["--noprogress".to_string(), "--nox11".to_string()];
        let args = make_args("MyApp", "myapp.run", None, "./setup.sh", &extra, None);
        assert!(args.contains(&"--noprogress".to_string()));
        assert!(args.contains(&"--nox11".to_string()));
    }

    #[test]
    fn test_make_args_packaging_date_emitted() {
        let args = make_args(
            "MyApp",
            "myapp.run",
            None,
            "./setup.sh",
            &[],
            Some("Tue May 20 14:30:00 UTC 2026"),
        );
        let idx = args
            .iter()
            .position(|a| a == "--packaging-date")
            .expect("expected --packaging-date in args");
        assert_eq!(args[idx + 1], "Tue May 20 14:30:00 UTC 2026");
    }

    #[test]
    fn test_make_args_packaging_date_omitted_when_none() {
        let args = make_args("MyApp", "myapp.run", None, "./setup.sh", &[], None);
        assert!(!args.iter().any(|a| a == "--packaging-date"));
    }

    #[test]
    #[serial_test::serial(env)]
    fn test_resolve_packaging_date_honors_sde() {
        // SAFETY: serialized via the env_source_date_epoch group used by
        // anodizer-core's `sde::tests`.
        unsafe { std::env::set_var("SOURCE_DATE_EPOCH", "1715000000") };
        let date = resolve_packaging_date().expect("packaging date under SDE");
        // 1715000000 = 2024-05-06 16:53:20 UTC; format mirrors `LC_ALL=C date -u`.
        assert!(date.contains("2024"), "date string: {date}");
        assert!(date.contains("UTC"), "date string: {date}");
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
    }

    #[test]
    #[serial_test::serial(env)]
    fn test_resolve_packaging_date_none_without_sde() {
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
        assert!(resolve_packaging_date().is_none());
    }

    #[test]
    fn test_group_by_platform() {
        let a1 = Artifact {
            kind: ArtifactKind::Binary,
            name: "myapp".to_string(),
            path: PathBuf::from("/dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        };
        let a2 = Artifact {
            kind: ArtifactKind::Binary,
            name: "myapp".to_string(),
            path: PathBuf::from("/dist/myapp-darwin"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        };
        let artifacts = vec![a1, a2];
        let groups = group_by_platform(&artifacts);
        assert_eq!(groups.len(), 2);
        assert!(groups.contains_key("linux_amd64"));
        assert!(groups.contains_key("darwin_arm64"));
    }

    #[test]
    fn test_makeself_stage_skips_empty_configs() {
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        let stage = MakeselfStage;
        // No makeself configs → no-op
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_makeself_config_parsing() {
        use anodizer_core::config::Config;

        let yaml = r#"
project_name: test
makeselfs:
  - id: default
    script: install.sh
    compression: xz
    goos:
      - linux
    files:
      - src: README.md
        dst: README.md
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.makeselfs.len(), 1);
        let ms = &config.makeselfs[0];
        assert_eq!(ms.id.as_deref(), Some("default"));
        assert_eq!(ms.script.as_deref(), Some("install.sh"));
        assert_eq!(ms.compression.as_deref(), Some("xz"));
        assert_eq!(ms.goos.as_ref().unwrap(), &["linux"]);
        assert_eq!(ms.files.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_makeself_config_single_object() {
        use anodizer_core::config::Config;

        let yaml = r#"
project_name: test
makeselfs:
  script: install.sh
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.makeselfs.len(), 1);
        assert_eq!(config.makeselfs[0].script.as_deref(), Some("install.sh"));
    }

    #[test]
    fn test_makeself_requires_script() {
        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            anodizer_core::context::ContextOptions::default(),
        );
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("test".to_string()),
            ..Default::default()
        }];
        let stage = MakeselfStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("script"),
            "should require script field"
        );
    }
}
