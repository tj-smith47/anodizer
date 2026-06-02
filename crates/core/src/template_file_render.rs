//! Shared per-entry rendering for the [`crate::config::TemplateFileConfig`]
//! shape — the field type behind both top-level `template_files:` and
//! archive-scoped `archives[].templated_files:`.
//!
//! Centralizes the skip → render-src → read-bytes → from_utf8 →
//! render-contents → render-dst → empty-skip → traversal-reject pipeline
//! so the top-level stage in `crates/stage-templatefiles/src/lib.rs` and
//! the archive-scoped renderer in `crates/stage-archive/src/run.rs` share
//! one implementation. Each caller appends its own write/registration
//! logic on top of the returned struct.

use std::path::Path;

use anyhow::{Context as _, Result, bail};

use crate::config::TemplateFileConfig;
use crate::context::Context;

/// Outcome of [`render_templated_file_entry`].
///
/// `rendered_dst` is the relative path the caller should resolve against
/// its output directory (the top-level stage uses `dist/`; the archive
/// stage uses a per-`(archive_id, target, format)` staging dir).
///
/// `rendered_contents` is the UTF-8 body to write at `rendered_dst`.
#[derive(Debug)]
pub struct TemplatedFileRender {
    pub rendered_dst: String,
    pub rendered_contents: String,
}

/// Render a single [`TemplateFileConfig`] entry.
///
/// Returns `Ok(None)` when the entry should be silently skipped:
/// either `entry.skip` evaluates truthy, or the rendered `dst` is the
/// empty string (matches GoReleaser's "Ignored if empty" contract).
///
/// `label_prefix` is the error-context prefix the caller wants on every
/// rendered error (e.g. `"templatefiles: id 'greeting'"` or
/// `"archives[default].templated_files[default]"`). It is woven into the
/// `with_context` chain so users see which entry blew up.
pub fn render_templated_file_entry(
    ctx: &Context,
    entry: &TemplateFileConfig,
    label_prefix: &str,
) -> Result<Option<TemplatedFileRender>> {
    if let Some(ref skip) = entry.skip {
        let off = skip
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("{label_prefix}: render skip template"))?;
        if off {
            return Ok(None);
        }
    }

    let rendered_src = ctx
        .render_template(&entry.src)
        .with_context(|| format!("{label_prefix}: render src path"))?;
    let src_path = Path::new(&rendered_src).to_path_buf();
    let src_bytes = std::fs::read(&src_path).with_context(|| {
        format!(
            "{label_prefix}: source file '{}' not found",
            src_path.display()
        )
    })?;
    let src_contents = std::str::from_utf8(&src_bytes).map_err(|_| {
        anyhow::anyhow!(
            "{label_prefix}: source file '{}' is not valid UTF-8 — \
             templated files must be text. Use a `files:` entry for binary contents.",
            src_path.display()
        )
    })?;
    let rendered_contents = ctx.render_template(src_contents).with_context(|| {
        format!(
            "{label_prefix}: render contents of '{}'",
            src_path.display()
        )
    })?;

    let rendered_dst = ctx
        .render_template(&entry.dst)
        .with_context(|| format!("{label_prefix}: render dst path"))?;
    if rendered_dst.is_empty() {
        return Ok(None);
    }
    if rendered_dst.contains("..") || Path::new(&rendered_dst).is_absolute() {
        bail!(
            "{label_prefix}: dst '{}' must be a relative path with no '..' segments",
            rendered_dst
        );
    }

    Ok(Some(TemplatedFileRender {
        rendered_dst,
        rendered_contents,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::TestContextBuilder;
    use tempfile::TempDir;

    fn build_ctx() -> Context {
        TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .build()
    }

    #[test]
    fn renders_contents_and_dst() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("input.tpl");
        std::fs::write(&src, "name={{ .ProjectName }} version={{ .Version }}").unwrap();

        let ctx = build_ctx();
        let entry = TemplateFileConfig {
            id: Some("e1".to_string()),
            src: src.to_string_lossy().to_string(),
            dst: "{{ .ProjectName }}.txt".to_string(),
            mode: None,
            skip: None,
        };

        let out = render_templated_file_entry(&ctx, &entry, "test")
            .unwrap()
            .expect("entry should render");
        assert_eq!(out.rendered_dst, "myapp.txt");
        assert_eq!(out.rendered_contents, "name=myapp version=1.0.0");
    }

    #[test]
    fn skip_eval_true_returns_none() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("input.tpl");
        std::fs::write(&src, "hi").unwrap();

        let ctx = build_ctx();
        let entry = TemplateFileConfig {
            id: None,
            src: src.to_string_lossy().to_string(),
            dst: "out.txt".to_string(),
            mode: None,
            skip: Some(crate::config::StringOrBool::Bool(true)),
        };

        let out = render_templated_file_entry(&ctx, &entry, "test").unwrap();
        assert!(out.is_none(), "skip=true must short-circuit to None");
    }

    #[test]
    fn empty_dst_returns_none() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("input.tpl");
        std::fs::write(&src, "hi").unwrap();

        let ctx = build_ctx();
        let entry = TemplateFileConfig {
            id: None,
            src: src.to_string_lossy().to_string(),
            // dst renders to empty (literal empty string) — must short-circuit.
            dst: String::new(),
            mode: None,
            skip: None,
        };

        let out = render_templated_file_entry(&ctx, &entry, "test").unwrap();
        assert!(out.is_none(), "empty rendered dst must short-circuit");
    }

    #[test]
    fn path_traversal_rejected() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("input.tpl");
        std::fs::write(&src, "hi").unwrap();

        let ctx = build_ctx();
        let entry = TemplateFileConfig {
            id: None,
            src: src.to_string_lossy().to_string(),
            dst: "../escape.txt".to_string(),
            mode: None,
            skip: None,
        };

        let err = render_templated_file_entry(&ctx, &entry, "test").unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("relative path") || chain.contains(".."),
            "expected traversal rejection, got {chain}"
        );
    }

    #[test]
    fn non_utf8_source_errors_clearly() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("binary.bin");
        std::fs::write(&src, [0xFF, 0xFE, 0x00, 0x80]).unwrap();

        let ctx = build_ctx();
        let entry = TemplateFileConfig {
            id: None,
            src: src.to_string_lossy().to_string(),
            dst: "out.txt".to_string(),
            mode: None,
            skip: None,
        };
        let err = render_templated_file_entry(&ctx, &entry, "test").unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("not valid UTF-8"),
            "expected UTF-8 error, got {chain}"
        );
    }

    #[test]
    fn missing_src_errors_with_label() {
        let ctx = build_ctx();
        let entry = TemplateFileConfig {
            id: None,
            src: "/nonexistent/file.tpl".to_string(),
            dst: "out.txt".to_string(),
            mode: None,
            skip: None,
        };
        let err = render_templated_file_entry(&ctx, &entry, "mylabel").unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("mylabel") && chain.contains("not found"),
            "expected label + 'not found' in chain, got {chain}"
        );
    }
}
