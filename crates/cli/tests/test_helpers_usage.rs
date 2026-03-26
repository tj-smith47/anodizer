//! Tests verifying that the shared test_helpers module from anodize-core
//! is usable from the CLI crate via dev-dependencies.

use anodize_core::test_helpers::{TestContextBuilder, make_git_info};

#[test]
fn test_context_builder_usable_from_cli_crate() {
    let ctx = TestContextBuilder::new()
        .project_name("cli-test")
        .tag("v3.1.0")
        .dry_run(true)
        .snapshot(false)
        .build();

    assert_eq!(
        ctx.template_vars().get("ProjectName"),
        Some(&"cli-test".to_string())
    );
    assert_eq!(
        ctx.template_vars().get("Tag"),
        Some(&"v3.1.0".to_string())
    );
    assert_eq!(
        ctx.template_vars().get("Version"),
        Some(&"3.1.0".to_string())
    );
    assert!(ctx.is_dry_run());
    assert!(!ctx.is_snapshot());
}

#[test]
fn test_make_git_info_usable_from_cli_crate() {
    let info = make_git_info(true, Some("rc.2"));
    assert!(info.dirty);
    assert_eq!(info.semver.prerelease, Some("rc.2".to_string()));
    assert_eq!(info.tag, "v1.2.3");
    assert_eq!(info.branch, "main");
}

#[test]
fn test_context_builder_template_rendering() {
    let ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .build();

    let rendered = ctx
        .render_template("{{ .ProjectName }}-{{ .Version }}")
        .unwrap();
    assert_eq!(rendered, "myapp-1.0.0");
}
