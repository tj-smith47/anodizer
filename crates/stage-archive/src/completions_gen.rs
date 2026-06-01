//! Turnkey completion / man-page generation for archive entries.
//!
//! Implements the three generation modes declared by
//! [`anodizer_core::config::CompletionsConfig`] /
//! [`anodizer_core::config::ManpagesConfig`] and stages the produced files in
//! a stable dist location so they feed BOTH the archive (via the existing
//! [`crate::file_specs::ResolvedExtraFile`] machinery) AND any nfpm package
//! whose `contents:` globs reach into the staging tree.
//!
//! Generation runs ONCE per crate (before the per-target archive loop):
//! completions/man pages do not vary by architecture, so the host-native
//! binary's output (mode A) — or the harvested / copied files (modes B/C) —
//! is reused for every archive across all targets.
//!
//! Staging layout (the single source of truth shared with nfpm):
//!
//! ```text
//! dist/.completions/<crate>/<file>     # e.g. dist/.completions/rg/rg.fish
//! dist/.manpages/<crate>/<file>        # e.g. dist/.manpages/rg/rg.1
//! ```
//!
//! nfpm `contents` can reference these directly:
//!
//! ```yaml
//! contents:
//!   - src: "dist/.completions/rg/*"
//!     dst: /usr/share/bash-completion/completions/
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::Artifact;
use anodizer_core::config::{CompletionsConfig, GenMode, ManpagesConfig, completion_filename};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

use crate::file_specs::ResolvedExtraFile;
use crate::formats::resolve_glob_patterns;

/// Subdirectory of `dist` holding generated completion files, keyed by crate.
const COMPLETIONS_STAGING: &str = ".completions";
/// Subdirectory of `dist` holding generated man files, keyed by crate.
const MANPAGES_STAGING: &str = ".manpages";

/// Generate (or harvest, or copy) completion and man files for a single
/// crate's archive config, returning [`ResolvedExtraFile`] entries to bundle
/// into every archive produced for the crate.
///
/// `host_binary` is the host-native built binary used by mode A — `None` when
/// no host-native artifact exists in the build matrix (a pure cross build).
/// Mode A errors clearly in that case rather than silently skipping; modes B
/// and C do not need it.
///
/// The returned files are staged on disk under `dist/.completions/<crate>/`
/// and `dist/.manpages/<crate>/` so the same files can also feed nfpm
/// `contents:` globs (the single source-of-truth requirement).
#[allow(clippy::too_many_arguments)]
pub(crate) fn generate_archive_aux_files(
    ctx: &mut Context,
    completions: Option<&CompletionsConfig>,
    manpages: Option<&ManpagesConfig>,
    crate_name: &str,
    crate_dir: &Path,
    host_binary: Option<&Artifact>,
    dist: &Path,
    dry_run: bool,
    log: &StageLogger,
) -> Result<Vec<ResolvedExtraFile>> {
    let mut out: Vec<ResolvedExtraFile> = Vec::new();

    if let Some(cfg) = completions {
        let staging = dist.join(COMPLETIONS_STAGING).join(crate_name);
        let files = gen_completions(
            ctx,
            cfg,
            crate_name,
            crate_dir,
            host_binary,
            &staging,
            dry_run,
            log,
        )?;
        append_entries(&mut out, files, cfg.resolved_dst());
    }

    if let Some(cfg) = manpages {
        let staging = dist.join(MANPAGES_STAGING).join(crate_name);
        let files = gen_manpages(
            ctx,
            cfg,
            crate_name,
            crate_dir,
            host_binary,
            &staging,
            dry_run,
            log,
        )?;
        append_entries(&mut out, files, cfg.resolved_dst());
    }

    Ok(out)
}

/// Wrap each staged file as a `ResolvedExtraFile` whose archive destination is
/// `<dst>/<filename>`. The `dst` directory is normalised to always end in `/`
/// so [`resolve_file_specs`]-style joining places the file inside it rather
/// than renaming.
fn append_entries(out: &mut Vec<ResolvedExtraFile>, files: Vec<PathBuf>, dst: &str) {
    let dst_dir = if dst.is_empty() || dst.ends_with('/') {
        dst.to_string()
    } else {
        format!("{dst}/")
    };
    for src in files {
        let file_name = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let archive_dst = format!("{dst_dir}{file_name}");
        out.push(ResolvedExtraFile {
            src,
            dst: Some(archive_dst),
            info: None,
            strip_parent: false,
            default: false,
        });
    }
}

/// Resolve which binary name to use in templates (`{{ .Binary }}`): the
/// host binary's recorded binary name, falling back to the crate name.
fn binary_name(host_binary: Option<&Artifact>, crate_name: &str) -> String {
    host_binary
        .and_then(|b| b.metadata.get("binary").cloned())
        .unwrap_or_else(|| crate_name.to_string())
}

#[allow(clippy::too_many_arguments)]
fn gen_completions(
    ctx: &mut Context,
    cfg: &CompletionsConfig,
    crate_name: &str,
    crate_dir: &Path,
    host_binary: Option<&Artifact>,
    staging: &Path,
    dry_run: bool,
    log: &StageLogger,
) -> Result<Vec<PathBuf>> {
    let bin = binary_name(host_binary, crate_name);
    match cfg.mode() {
        GenMode::None => Ok(Vec::new()),
        GenMode::Generate(cmd_tmpl) => {
            let host = host_binary.ok_or_else(|| host_missing_error(crate_name, "completions"))?;
            let mut files = Vec::new();
            for shell in cfg.resolved_shells() {
                let file_name = completion_filename(&bin, &shell);
                let path = run_generate(
                    ctx,
                    cmd_tmpl,
                    host,
                    &bin,
                    Some(&shell),
                    staging,
                    &file_name,
                    dry_run,
                    log,
                    "completions",
                )?;
                files.push(path);
            }
            Ok(files)
        }
        GenMode::FromBuildOut(glob_tmpl) => {
            harvest_from_build_out(ctx, glob_tmpl, &bin, staging, log, "completions")
        }
        GenMode::Copy(glob_tmpl) => {
            copy_committed(ctx, glob_tmpl, &bin, crate_dir, staging, log, "completions")
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn gen_manpages(
    ctx: &mut Context,
    cfg: &ManpagesConfig,
    crate_name: &str,
    crate_dir: &Path,
    host_binary: Option<&Artifact>,
    staging: &Path,
    dry_run: bool,
    log: &StageLogger,
) -> Result<Vec<PathBuf>> {
    let bin = binary_name(host_binary, crate_name);
    match cfg.mode() {
        GenMode::None => Ok(Vec::new()),
        GenMode::Generate(cmd_tmpl) => {
            let host = host_binary.ok_or_else(|| host_missing_error(crate_name, "manpages"))?;
            // Man pages are shell-agnostic: one file named `<binary>.1`.
            let file_name = format!("{bin}.1");
            let path = run_generate(
                ctx, cmd_tmpl, host, &bin, None, staging, &file_name, dry_run, log, "manpages",
            )?;
            Ok(vec![path])
        }
        GenMode::FromBuildOut(glob_tmpl) => {
            harvest_from_build_out(ctx, glob_tmpl, &bin, staging, log, "manpages")
        }
        GenMode::Copy(glob_tmpl) => {
            copy_committed(ctx, glob_tmpl, &bin, crate_dir, staging, log, "manpages")
        }
    }
}

/// The clear error emitted when mode A is requested but no host-native
/// artifact exists in the build matrix (pure cross build).
fn host_missing_error(crate_name: &str, kind: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "archive {kind}.generate (mode A) for crate '{crate_name}' requires a host-native \
         binary to run, but no built artifact matches the host target. Either add the host \
         target to your build matrix, or switch to `from_build_out:` (harvest a build.rs \
         OUT_DIR) / `copy:` (copy committed files)."
    )
}

/// Mode A: render the `generate:` command (binding `{{ .Shell }}` /
/// `{{ .Binary }}` / `{{ .ArtifactPath }}`), run it once via `sh -c`, and
/// capture stdout into `staging/<file_name>`.
#[allow(clippy::too_many_arguments)]
fn run_generate(
    ctx: &mut Context,
    cmd_tmpl: &str,
    host: &Artifact,
    bin: &str,
    shell: Option<&str>,
    staging: &Path,
    file_name: &str,
    dry_run: bool,
    log: &StageLogger,
    kind: &str,
) -> Result<PathBuf> {
    std::fs::create_dir_all(staging)
        .with_context(|| format!("{kind}: create staging dir {}", staging.display()))?;
    let out_path = staging.join(file_name);

    // Bind the mode-A template surface, then render the command.
    let tvars = ctx.template_vars_mut();
    tvars.set("ArtifactPath", &host.path.to_string_lossy());
    tvars.set("Binary", bin);
    if let Some(s) = shell {
        tvars.set("Shell", s);
    }
    let cmd = ctx
        .render_template(cmd_tmpl)
        .with_context(|| format!("{kind}: render generate command '{cmd_tmpl}'"))?;

    if dry_run {
        log.status(&format!(
            "(dry-run) would generate {kind} via `{cmd}` -> {}",
            out_path.display()
        ));
        // Write a placeholder so downstream bundling has a real file in
        // dry-run mode (the archive stage still skips writing under dry-run).
        std::fs::write(&out_path, b"")
            .with_context(|| format!("{kind}: write dry-run placeholder {}", out_path.display()))?;
        return Ok(out_path);
    }

    log.status(&format!("generating {kind}: {cmd}"));
    let output = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .output()
        .with_context(|| format!("{kind}: spawn generate command `{cmd}`"))?;
    if !output.status.success() {
        bail!(
            "{kind}: generate command `{cmd}` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    std::fs::write(&out_path, &output.stdout)
        .with_context(|| format!("{kind}: write generated file {}", out_path.display()))?;
    Ok(out_path)
}

/// Mode B: render the per-target glob (binding `{{ .Binary }}`), expand it,
/// and copy every matched file into `staging` (flattened to its basename).
fn harvest_from_build_out(
    ctx: &mut Context,
    glob_tmpl: &str,
    bin: &str,
    staging: &Path,
    log: &StageLogger,
    kind: &str,
) -> Result<Vec<PathBuf>> {
    ctx.template_vars_mut().set("Binary", bin);
    let glob = ctx
        .render_template(glob_tmpl)
        .with_context(|| format!("{kind}: render from_build_out glob '{glob_tmpl}'"))?;
    let matched = glob_with_braces(&glob)
        .with_context(|| format!("{kind}: expand from_build_out glob '{glob}'"))?;
    stage_files(&matched, staging, log, kind, &glob)
}

/// Expand any `{a,b,c}` brace alternations in `pattern` into concrete glob
/// patterns, then run each through [`resolve_glob_patterns`]. The `glob` crate
/// does not understand brace alternation, but the canonical clap_complete
/// build-out pattern (`**/out/<bin>.{bash,fish,zsh}`) relies on it, so we
/// pre-expand here. Nested/multiple groups are handled by recursing on the
/// remaining alternations. Patterns with no braces pass straight through.
fn glob_with_braces(pattern: &str) -> Result<Vec<PathBuf>> {
    let mut results = Vec::new();
    for expanded in expand_braces(pattern) {
        results.extend(resolve_glob_patterns(std::slice::from_ref(&expanded))?);
    }
    Ok(results)
}

/// Recursively expand the first `{a,b,...}` group in `pattern` into one string
/// per alternative, leaving non-brace text intact. Returns `[pattern]` when no
/// (well-formed) brace group is present.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let Some(rel_close) = pattern[open..].find('}') else {
        return vec![pattern.to_string()];
    };
    let close = open + rel_close;
    let prefix = &pattern[..open];
    let body = &pattern[open + 1..close];
    let suffix = &pattern[close + 1..];
    let mut out = Vec::new();
    for alt in body.split(',') {
        // Recurse so additional groups in the suffix are expanded too.
        for tail in expand_braces(suffix) {
            out.push(format!("{prefix}{alt}{tail}"));
        }
    }
    out
}

/// Mode C: render the copy glob (binding `{{ .Binary }}`), expand it relative
/// to the crate dir when not absolute, and copy matches into `staging`.
fn copy_committed(
    ctx: &mut Context,
    glob_tmpl: &str,
    bin: &str,
    crate_dir: &Path,
    staging: &Path,
    log: &StageLogger,
    kind: &str,
) -> Result<Vec<PathBuf>> {
    ctx.template_vars_mut().set("Binary", bin);
    let rendered = ctx
        .render_template(glob_tmpl)
        .with_context(|| format!("{kind}: render copy glob '{glob_tmpl}'"))?;
    // Resolve relative globs against the crate root (mirrors how
    // resolve_default_extra_files anchors LICENSE/README globs).
    let glob = if Path::new(&rendered).is_absolute() {
        rendered
    } else {
        crate_dir.join(&rendered).to_string_lossy().to_string()
    };
    let matched =
        glob_with_braces(&glob).with_context(|| format!("{kind}: expand copy glob '{glob}'"))?;
    stage_files(&matched, staging, log, kind, &glob)
}

/// Copy each matched file into `staging` under its basename, returning the
/// staged paths. Warns (does not error) when the glob matched nothing, so a
/// build that conditionally emits completions does not hard-fail snapshots.
fn stage_files(
    matched: &[PathBuf],
    staging: &Path,
    log: &StageLogger,
    kind: &str,
    glob: &str,
) -> Result<Vec<PathBuf>> {
    if matched.is_empty() {
        log.warn(&format!(
            "{kind}: glob '{glob}' matched no files — no {kind} bundled"
        ));
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(staging)
        .with_context(|| format!("{kind}: create staging dir {}", staging.display()))?;
    let mut staged = Vec::with_capacity(matched.len());
    for src in matched {
        let file_name = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let dest = staging.join(&file_name);
        std::fs::copy(src, &dest)
            .with_context(|| format!("{kind}: copy {} -> {}", src.display(), dest.display()))?;
        staged.push(dest);
    }
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::expand_braces;

    #[test]
    fn expands_single_group() {
        let got = expand_braces("**/out/rg.{bash,fish,zsh}");
        assert_eq!(
            got,
            vec![
                "**/out/rg.bash".to_string(),
                "**/out/rg.fish".to_string(),
                "**/out/rg.zsh".to_string(),
            ]
        );
    }

    #[test]
    fn no_braces_passthrough() {
        assert_eq!(
            expand_braces("contrib/completion/*"),
            vec!["contrib/completion/*".to_string()]
        );
    }

    #[test]
    fn unclosed_brace_passthrough() {
        assert_eq!(expand_braces("a{b,c"), vec!["a{b,c".to_string()]);
    }

    #[test]
    fn two_groups_cartesian() {
        let got = expand_braces("{a,b}/{1,2}");
        assert_eq!(
            got,
            vec![
                "a/1".to_string(),
                "a/2".to_string(),
                "b/1".to_string(),
                "b/2".to_string(),
            ]
        );
    }
}
