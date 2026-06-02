//! Shared utility for processing `templated_extra_files` across stages.
//!
//! Unlike regular `extra_files` (which copy files as-is), `templated_extra_files`
//! reads file contents, renders them through the template engine, and writes the
//! rendered output. This is a GoReleaser Pro feature.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};

use crate::config::TemplatedExtraFile;
use crate::context::Context;
use crate::template::TemplateVars;

/// Process templated_extra_files: read each src, render contents through
/// the template engine, write rendered output to `output_dir/dst`.
/// Returns list of `(output_path, dst_name)` pairs.
pub fn process_templated_extra_files(
    specs: &[TemplatedExtraFile],
    ctx: &Context,
    output_dir: &Path,
    stage_name: &str,
) -> anyhow::Result<Vec<(PathBuf, String)>> {
    process_templated_extra_files_with_vars(specs, ctx.template_vars(), output_dir, stage_name)
}

/// Like [`process_templated_extra_files`] but accepts raw [`TemplateVars`]
/// instead of a full [`Context`]. Used by the publisher stage which does not
/// have access to a Context.
pub fn process_templated_extra_files_with_vars(
    specs: &[TemplatedExtraFile],
    vars: &TemplateVars,
    output_dir: &Path,
    stage_name: &str,
) -> anyhow::Result<Vec<(PathBuf, String)>> {
    let render_fn = |tmpl: &str| -> anyhow::Result<String> { crate::template::render(tmpl, vars) };

    let mut results = Vec::new();
    for spec in specs {
        // Read source file
        let src_content = std::fs::read_to_string(&spec.src)
            .with_context(|| format!("{}: read templated file '{}'", stage_name, spec.src))?;
        // Render contents through template engine
        let rendered = render_fn(&src_content)
            .with_context(|| format!("{}: render templated file '{}'", stage_name, spec.src))?;
        // Determine destination name (render dst through template engine if set)
        let dst_name = if let Some(ref d) = spec.dst {
            render_fn(d)
                .with_context(|| format!("{}: render dst for '{}'", stage_name, spec.src))?
        } else {
            let name = Path::new(&spec.src)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            name.strip_suffix(".tpl").unwrap_or(&name).to_string()
        };

        // Reject path traversal: dst must be a relative path with no ".." components
        if dst_name.contains("..") || Path::new(&dst_name).is_absolute() {
            bail!(
                "{}: templated_extra_files dst '{}' must be a relative path within output directory (no '..' or absolute paths)",
                stage_name,
                dst_name
            );
        }

        // Write to output directory
        let output_path = output_dir.join(&dst_name);
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&output_path, &rendered).with_context(|| {
            format!(
                "{}: write templated file '{}'",
                stage_name,
                output_path.display()
            )
        })?;

        // Apply file mode if specified (unix only)
        #[cfg(unix)]
        if let Some(mode_str) = &spec.mode {
            if let Some(mode_val) = crate::config::parse_octal_mode(mode_str) {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&output_path, std::fs::Permissions::from_mode(mode_val))
                    .with_context(|| {
                    format!("{}: set mode on '{}'", stage_name, output_path.display())
                })?;
            } else {
                bail!(
                    "{}: invalid mode '{}' for templated file '{}'",
                    stage_name,
                    mode_str,
                    spec.src
                );
            }
        }

        results.push((output_path, dst_name));
    }
    Ok(results)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::TestContextBuilder;

    #[test]
    fn test_renders_template_variables() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("readme.tpl");
        std::fs::write(&src, "Project: {{ .ProjectName }}, Version: {{ .Version }}").unwrap();

        let ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.2.3")
            .build();

        let out_dir = tmp.path().join("output");
        let specs = vec![TemplatedExtraFile {
            src: src.to_string_lossy().to_string(),
            dst: Some("README.txt".to_string()),
            mode: None,
        }];

        let results = process_templated_extra_files(&specs, &ctx, &out_dir, "test").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "README.txt");

        let content = std::fs::read_to_string(&results[0].0).unwrap();
        assert_eq!(content, "Project: myapp, Version: 1.2.3");
    }

    #[test]
    fn test_default_dst_strips_tpl_extension() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("LICENSE.tpl");
        std::fs::write(&src, "MIT License").unwrap();

        let ctx = TestContextBuilder::new().build();
        let out_dir = tmp.path().join("output");
        let specs = vec![TemplatedExtraFile {
            src: src.to_string_lossy().to_string(),
            dst: None,
            mode: None,
        }];

        let results = process_templated_extra_files(&specs, &ctx, &out_dir, "test").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "LICENSE");
    }

    #[test]
    fn test_default_dst_preserves_name_without_tpl() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("NOTICE.md");
        std::fs::write(&src, "Notice content").unwrap();

        let ctx = TestContextBuilder::new().build();
        let out_dir = tmp.path().join("output");
        let specs = vec![TemplatedExtraFile {
            src: src.to_string_lossy().to_string(),
            dst: None,
            mode: None,
        }];

        let results = process_templated_extra_files(&specs, &ctx, &out_dir, "test").unwrap();
        assert_eq!(results[0].1, "NOTICE.md");
    }

    #[test]
    fn test_custom_dst_name_used() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("template.txt");
        std::fs::write(&src, "content").unwrap();

        let ctx = TestContextBuilder::new().build();
        let out_dir = tmp.path().join("output");
        let specs = vec![TemplatedExtraFile {
            src: src.to_string_lossy().to_string(),
            dst: Some("custom-output.txt".to_string()),
            mode: None,
        }];

        let results = process_templated_extra_files(&specs, &ctx, &out_dir, "test").unwrap();
        assert_eq!(results[0].1, "custom-output.txt");
        assert!(results[0].0.ends_with("custom-output.txt"));
    }

    #[test]
    fn test_error_on_missing_src() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = TestContextBuilder::new().build();
        let out_dir = tmp.path().join("output");
        let specs = vec![TemplatedExtraFile {
            src: "/nonexistent/path/file.tpl".to_string(),
            dst: None,
            mode: None,
        }];

        let result = process_templated_extra_files(&specs, &ctx, &out_dir, "release");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("release"), "error should mention stage name");
        assert!(
            err.contains("nonexistent"),
            "error should mention the missing file"
        );
    }

    #[test]
    fn test_empty_specs_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = TestContextBuilder::new().build();
        let out_dir = tmp.path().join("output");

        let results = process_templated_extra_files(&[], &ctx, &out_dir, "test").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_path_traversal_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("evil.tpl");
        std::fs::write(&src, "malicious").unwrap();

        let ctx = TestContextBuilder::new().build();
        let out_dir = tmp.path().join("output");

        // Relative path with ".." should be rejected
        let specs = vec![TemplatedExtraFile {
            src: src.to_string_lossy().to_string(),
            dst: Some("../../etc/cron.d/evil".to_string()),
            mode: None,
        }];
        let result = process_templated_extra_files(&specs, &ctx, &out_dir, "test");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains(".."), "error should mention '..'");
        assert!(
            err.contains("relative path"),
            "error should explain constraint"
        );
    }

    #[test]
    fn test_absolute_dst_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("evil.tpl");
        std::fs::write(&src, "malicious").unwrap();

        let ctx = TestContextBuilder::new().build();
        let out_dir = tmp.path().join("output");

        // Use a platform-appropriate absolute path
        let absolute_dst = if cfg!(windows) {
            "C:\\etc\\passwd".to_string()
        } else {
            "/etc/passwd".to_string()
        };

        let specs = vec![TemplatedExtraFile {
            src: src.to_string_lossy().to_string(),
            dst: Some(absolute_dst),
            mode: None,
        }];
        let result = process_templated_extra_files(&specs, &ctx, &out_dir, "test");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("absolute"),
            "error should mention absolute paths"
        );
    }

    #[test]
    fn test_dst_rendered_through_template_engine() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("notes.tpl");
        std::fs::write(&src, "Release notes").unwrap();

        let ctx = TestContextBuilder::new().project_name("myapp").build();
        let out_dir = tmp.path().join("output");

        let specs = vec![TemplatedExtraFile {
            src: src.to_string_lossy().to_string(),
            dst: Some("{{ .ProjectName }}-NOTES.txt".to_string()),
            mode: None,
        }];

        let results = process_templated_extra_files(&specs, &ctx, &out_dir, "test").unwrap();
        assert_eq!(results[0].1, "myapp-NOTES.txt");
        assert!(results[0].0.ends_with("myapp-NOTES.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn test_mode_applied_to_output_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("script.tpl");
        std::fs::write(&src, "#!/bin/sh\necho hello").unwrap();

        let ctx = TestContextBuilder::new().build();
        let out_dir = tmp.path().join("output");

        let specs = vec![TemplatedExtraFile {
            src: src.to_string_lossy().to_string(),
            dst: Some("run.sh".to_string()),
            mode: Some("0755".to_string()),
        }];

        let results = process_templated_extra_files(&specs, &ctx, &out_dir, "test").unwrap();
        assert_eq!(results.len(), 1);

        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::metadata(&results[0].0).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o755);
    }

    #[test]
    fn test_invalid_mode_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("file.tpl");
        std::fs::write(&src, "content").unwrap();

        let ctx = TestContextBuilder::new().build();
        let out_dir = tmp.path().join("output");

        let specs = vec![TemplatedExtraFile {
            src: src.to_string_lossy().to_string(),
            dst: None,
            mode: Some("notamode".to_string()),
        }];

        // On unix, invalid mode should error. On non-unix, mode is ignored.
        let result = process_templated_extra_files(&specs, &ctx, &out_dir, "test");
        if cfg!(unix) {
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(err.contains("invalid mode"));
        } else {
            assert!(result.is_ok());
        }
    }

    #[test]
    fn test_multiple_files_processed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src1 = tmp.path().join("a.tpl");
        let src2 = tmp.path().join("b.txt");
        std::fs::write(&src1, "File A: {{ .ProjectName }}").unwrap();
        std::fs::write(&src2, "File B: {{ .Version }}").unwrap();

        let ctx = TestContextBuilder::new()
            .project_name("multi")
            .tag("v2.0.0")
            .build();
        let out_dir = tmp.path().join("output");
        let specs = vec![
            TemplatedExtraFile {
                src: src1.to_string_lossy().to_string(),
                dst: None,
                mode: None,
            },
            TemplatedExtraFile {
                src: src2.to_string_lossy().to_string(),
                dst: Some("renamed.txt".to_string()),
                mode: None,
            },
        ];

        let results = process_templated_extra_files(&specs, &ctx, &out_dir, "test").unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].1, "a"); // .tpl stripped
        assert_eq!(results[1].1, "renamed.txt");

        let content1 = std::fs::read_to_string(&results[0].0).unwrap();
        assert_eq!(content1, "File A: multi");
        let content2 = std::fs::read_to_string(&results[1].0).unwrap();
        assert_eq!(content2, "File B: 2.0.0");
    }
}
