//! `SourceStage` orchestration: source archive emission and SBOM generation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::SourceFileEntry;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

use crate::archive::{SourceArchiveInputs, create_source_archive, get_repo_root};

pub struct SourceStage;

impl Stage for SourceStage {
    fn name(&self) -> &str {
        "source"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("source");
        let source_enabled = ctx
            .config
            .source
            .as_ref()
            .map(|s| s.is_enabled())
            .unwrap_or(false);

        if !source_enabled {
            log.status("source archive not enabled, skipping");
            return Ok(());
        }

        let dist = ctx.config.dist.clone();
        if !ctx.is_dry_run() {
            std::fs::create_dir_all(&dist).with_context(|| {
                format!("source: failed to create dist dir: {}", dist.display())
            })?;
        }

        self.run_source_archive(ctx, &dist)?;

        Ok(())
    }
}

impl SourceStage {
    fn run_source_archive(&self, ctx: &mut Context, dist: &Path) -> Result<()> {
        let source_cfg = ctx
            .config
            .source
            .as_ref()
            .context("source stage invoked without source config (programmer bug)")?;
        let format = source_cfg.archive_format().to_string();

        // Determine the archive name. Cloned (not borrowed) so the later
        // `template_vars_mut()` write for `SourcePrefix` does not collide with
        // a live immutable borrow of `ctx`.
        let project_name = ctx.config.project_name.clone();
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        let name = if let Some(ref tpl) = source_cfg.name_template {
            ctx.render_template(tpl)
                .with_context(|| format!("source: failed to render name_template '{}'", tpl))?
        } else {
            format!("{}-{}", project_name, version)
        };
        // The rendered `name` becomes the source-archive filename stem
        // (`{name}.{format}` written under `dist/`). An empty stem would
        // produce a hidden file like `dist/.tar.gz`, which `git archive`
        // happily writes but downstream stages (checksum, sign, release
        // upload) cannot locate by canonical name. Bail with an actionable
        // hint instead of silently writing a hidden artifact.
        if name.is_empty() {
            anyhow::bail!(
                "source: rendered source archive name is empty. The configured \
                 `source.name_template` rendered to '' (or both `project_name` \
                 and Version were empty when the template fell back to the \
                 `<project>-<version>` default). An empty name produces a \
                 hidden output path (`dist/.{}`) that downstream stages \
                 (checksum, sign, release) cannot resolve. Set \
                 `source.name_template:` explicitly or verify `project_name` is \
                 populated in the config.",
                format,
            );
        }

        // Determine the archive prefix (directory name inside the archive).
        // Defaults to empty (no prefix) when prefix_template is not configured.
        let prefix = if let Some(ref tpl) = source_cfg.prefix_template {
            ctx.render_template(tpl)
                .with_context(|| format!("source: failed to render prefix_template '{}'", tpl))?
        } else {
            String::new()
        };

        // The source archive has a real top-level directory IFF the rendered
        // prefix ends with `/` — that is the only form `git archive --prefix`
        // treats as a directory. A slash-less prefix (`foo`) is glued onto
        // every path (`foomain.rs`), yielding a FLAT archive with no top dir,
        // exactly like an empty prefix. `SourcePrefix` therefore means "the
        // archive's top-level directory, or empty when the archive is flat":
        // the dir name (slash stripped) for a trailing-slash prefix, else
        // empty. The empty case routes the srpm `%autosetup` to `-c`, which is
        // correct for both flat shapes. Owned so the `source_cfg` borrow can
        // end before the `template_vars_mut()` write.
        let source_prefix = prefix
            .strip_suffix('/')
            .map(str::to_string)
            .unwrap_or_default();

        let log = ctx.logger("source");

        let cwd = ctx
            .options
            .project_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let repo_root = get_repo_root(&cwd, &log)?;

        // Render and expand extra-file globs up front, even in dry-run mode,
        // so users catch template typos and zero-match patterns before the
        // real run.
        let mut extra_files: Vec<SourceFileEntry> = Vec::new();
        for entry in &source_cfg.files {
            let rendered_src = ctx.render_template(&entry.src).with_context(|| {
                format!("source: render extra files src template '{}'", entry.src)
            })?;

            let pattern = if Path::new(&rendered_src).is_absolute() {
                rendered_src.clone()
            } else {
                repo_root.join(&rendered_src).to_string_lossy().into_owned()
            };

            let expanded_for_entry = match glob::glob(&pattern) {
                Ok(paths) => {
                    let expanded: Vec<_> = paths
                        .filter_map(|p| p.ok())
                        .filter(|p| p.is_file())
                        .map(|p| SourceFileEntry {
                            src: p.to_string_lossy().into_owned(),
                            dst: entry.dst.clone(),
                            strip_parent: entry.strip_parent,
                            info: entry.info.clone(),
                        })
                        .collect();
                    if expanded.is_empty() {
                        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
                            log.warn(&format!("extra file pattern {pattern:?} matched no files"));
                        }
                        vec![SourceFileEntry {
                            src: rendered_src,
                            dst: entry.dst.clone(),
                            strip_parent: entry.strip_parent,
                            info: entry.info.clone(),
                        }]
                    } else {
                        expanded
                    }
                }
                Err(e) => {
                    log.warn(&format!(
                        "extra file pattern {pattern:?} is not a valid glob ({e}); \
                         treating as literal path"
                    ));
                    vec![SourceFileEntry {
                        src: rendered_src,
                        dst: entry.dst.clone(),
                        strip_parent: entry.strip_parent,
                        info: entry.info.clone(),
                    }]
                }
            };
            extra_files.extend(expanded_for_entry);
        }

        // Publish the archive's top-level directory so later stages and user
        // specs can reference `{{ SourcePrefix }}` — notably an srpm
        // `%autosetup -n`, which must `cd` into the exact dir the tarball
        // contains. Derived from the RAW `Version`, so it is unaffected by the
        // srpm stage's scoped sanitized-`Version` override. Set before the
        // dry-run gate so the var is available even when no file is written.
        ctx.template_vars_mut().set("SourcePrefix", &source_prefix);

        if ctx.is_dry_run() {
            log.status(&format!(
                "(dry-run) would create {}.{} archive",
                name, format
            ));
            return Ok(());
        }

        log.status(&format!("creating {}.{} archive...", name, format));
        // The source archive always passes
        // `ctx.Git.FullCommit` (the resolved SHA) to `git archive`, never the
        // literal `HEAD` ref. When `git_info` was not pre-populated by the
        // git pipe (e.g. local `anodizer release --snapshot`), resolve HEAD
        // ourselves via the allow-listed `core::git::get_head_commit` helper
        // so the source archive is deterministic across consecutive commits.
        let resolved_commit: String;
        let commit: &str = match ctx.git_info.as_ref() {
            Some(info) if !info.commit.is_empty() => info.commit.as_str(),
            _ => {
                resolved_commit = anodizer_core::git::get_head_commit()
                    .with_context(|| "source: failed to resolve HEAD via `git rev-parse HEAD`")?;
                resolved_commit.as_str()
            }
        };
        let sde_mtime = ctx
            .env_var("SOURCE_DATE_EPOCH")
            .and_then(|s| s.parse::<u64>().ok());
        let output_path = create_source_archive(&SourceArchiveInputs {
            dist,
            format: &format,
            name: &name,
            prefix: &prefix,
            extra_files: &extra_files,
            repo_root: &repo_root,
            commit,
            log: &log,
            strict: ctx.is_strict(),
            sde_mtime,
        })?;

        // The artifact name is the filename (e.g. "foo-1.0.0.tar.gz").
        let artifact_name = output_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let mut metadata = HashMap::new();
        metadata.insert("format".to_string(), format);

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::SourceArchive,
            name: artifact_name,
            path: output_path,
            target: None,
            crate_name: project_name.clone(),
            metadata,
            size: None,
        });

        Ok(())
    }
}
