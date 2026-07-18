#![allow(clippy::field_reassign_with_default)]

use super::*;

// -----------------------------------------------------------------------
// generate_pkgbuild tests
// -----------------------------------------------------------------------

#[test]
fn test_generate_pkgbuild_basic() {
    let pkgbuild = generate_pkgbuild(&PkgbuildParams {
        name: "mytool",
        version: "1.0.0",
        pkgrel: 1,
        description: "A great tool",
        url: "https://github.com/org/mytool",
        license: &["MIT".to_string()],
        maintainers: &["Jane Doe <jane@example.com>".to_string()],
        contributors: &[],
        depends: &[],
        optdepends: &[],
        conflicts: &[],
        provides: &[],
        replaces: &[],
        backup: &[],
        sources: &[(
            "x86_64".to_string(),
            "https://example.com/mytool-1.0.0-linux-amd64.tar.gz".to_string(),
            "deadbeef1234".to_string(),
        )],
        binary_name: "mytool",
        install_template: None,
        extra_install_lines: &[],
        install_file: None,
    })
    .unwrap();

    assert!(pkgbuild.contains("# Maintainer: Jane Doe <jane@example.com>"));
    assert!(pkgbuild.contains("pkgname='mytool'"));
    assert!(pkgbuild.contains("pkgver=1.0.0"));
    assert!(pkgbuild.contains("pkgrel=1"));
    assert!(pkgbuild.contains("pkgdesc=\"A great tool\""));
    assert!(pkgbuild.contains("arch=('x86_64')"));
    assert!(pkgbuild.contains("url=\"https://github.com/org/mytool\""));
    assert!(pkgbuild.contains("license=('MIT')"));
    assert!(pkgbuild.contains("depends=()"));
    assert!(pkgbuild.contains(
            "source_x86_64=(\"mytool_${pkgver}_x86_64.tar.gz::https://example.com/mytool-${pkgver}-linux-amd64.tar.gz\")"
        ));
    assert!(pkgbuild.contains("sha256sums_x86_64=('deadbeef1234')"));
    assert!(pkgbuild.contains("package()"));
    assert!(pkgbuild.contains("install -Dm755 \"$srcdir/mytool\" \"$pkgdir/usr/bin/mytool\""));
}

#[test]
fn test_generate_pkgbuild_multi_arch() {
    let pkgbuild = generate_pkgbuild(&PkgbuildParams {
        name: "mytool",
        version: "2.0.0",
        pkgrel: 1,
        description: "Multi-arch tool",
        url: "https://github.com/org/mytool",
        license: &["Apache-2.0".to_string()],
        maintainers: &[],
        contributors: &[],
        depends: &[],
        optdepends: &[],
        conflicts: &[],
        provides: &[],
        replaces: &[],
        backup: &[],
        sources: &[
            (
                "x86_64".to_string(),
                "https://example.com/mytool-2.0.0-linux-amd64.tar.gz".to_string(),
                "hash_amd64".to_string(),
            ),
            (
                "aarch64".to_string(),
                "https://example.com/mytool-2.0.0-linux-arm64.tar.gz".to_string(),
                "hash_arm64".to_string(),
            ),
        ],
        binary_name: "mytool",
        install_template: None,
        extra_install_lines: &[],
        install_file: None,
    })
    .unwrap();

    assert!(pkgbuild.contains("arch=('aarch64' 'x86_64')"));
    assert!(pkgbuild.contains("source_x86_64="));
    assert!(pkgbuild.contains("source_aarch64="));
    assert!(pkgbuild.contains("sha256sums_x86_64=('hash_amd64')"));
    assert!(pkgbuild.contains("sha256sums_aarch64=('hash_arm64')"));
}

#[test]
fn test_generate_pkgbuild_with_depends() {
    let pkgbuild = generate_pkgbuild(&PkgbuildParams {
        name: "mytool",
        version: "1.0.0",
        pkgrel: 1,
        description: "A tool",
        url: "https://example.com",
        license: &["MIT".to_string()],
        maintainers: &[],
        contributors: &[],
        depends: &["glibc".to_string(), "openssl".to_string()],
        optdepends: &["git: for VCS support".to_string()],
        conflicts: &["mytool-git".to_string()],
        provides: &["mytool".to_string()],
        replaces: &["old-mytool".to_string()],
        backup: &["etc/mytool/config.toml".to_string()],
        sources: &[(
            "x86_64".to_string(),
            "https://example.com/mytool.tar.gz".to_string(),
            "hash".to_string(),
        )],
        binary_name: "mytool",
        install_template: None,
        extra_install_lines: &[],
        install_file: None,
    })
    .unwrap();

    assert!(pkgbuild.contains("depends=('glibc' 'openssl')"));
    assert!(pkgbuild.contains("optdepends=('git: for VCS support')"));
    assert!(pkgbuild.contains("conflicts=('mytool-git')"));
    assert!(pkgbuild.contains("provides=('mytool')"));
    assert!(pkgbuild.contains("replaces=('old-mytool')"));
    assert!(pkgbuild.contains("backup=('etc/mytool/config.toml')"));
}

#[test]
fn test_generate_pkgbuild_no_maintainers() {
    let pkgbuild = generate_pkgbuild(&PkgbuildParams {
        name: "tool",
        version: "1.0.0",
        pkgrel: 1,
        description: "A tool",
        url: "https://example.com",
        license: &["MIT".to_string()],
        maintainers: &[],
        contributors: &[],
        depends: &[],
        optdepends: &[],
        conflicts: &[],
        provides: &[],
        replaces: &[],
        backup: &[],
        sources: &[(
            "x86_64".to_string(),
            "https://example.com/tool.tar.gz".to_string(),
            "hash".to_string(),
        )],
        binary_name: "tool",
        install_template: None,
        extra_install_lines: &[],
        install_file: None,
    })
    .unwrap();

    assert!(!pkgbuild.contains("# Maintainer:"));
    assert!(pkgbuild.starts_with("pkgname="));
}

#[test]
fn test_generate_pkgbuild_complete_structure() {
    let pkgbuild = generate_pkgbuild(&PkgbuildParams {
            name: "anodizer",
            version: "3.2.1",
            pkgrel: 1,
            description: "Release automation for Rust projects",
            url: "https://github.com/tj-smith47/anodizer",
            license: &["Apache-2.0".to_string()],
            maintainers: &["TJ Smith <tj@example.com>".to_string()],
            contributors: &[],
            depends: &[],
            optdepends: &[],
            conflicts: &[],
            provides: &[],
            replaces: &[],
            backup: &[],
            sources: &[
                (
                    "x86_64".to_string(),
                    "https://github.com/tj-smith47/anodizer/releases/download/v3.2.1/anodizer-3.2.1-linux-amd64.tar.gz".to_string(),
                    "aabbccdd".to_string(),
                ),
                (
                    "aarch64".to_string(),
                    "https://github.com/tj-smith47/anodizer/releases/download/v3.2.1/anodizer-3.2.1-linux-arm64.tar.gz".to_string(),
                    "eeff0011".to_string(),
                ),
            ],
            binary_name: "anodizer",
            install_template: None,
            extra_install_lines: &[],
            install_file: None,
        }).unwrap();

    // Starts with maintainer comment
    assert!(pkgbuild.starts_with("# Maintainer: TJ Smith <tj@example.com>"));

    // Contains required fields
    assert!(pkgbuild.contains("pkgname='anodizer'"));
    assert!(pkgbuild.contains("pkgver=3.2.1"));
    assert!(pkgbuild.contains("arch=('aarch64' 'x86_64')"));

    // Contains package() function
    assert!(pkgbuild.contains("package() {"));
    assert!(pkgbuild.contains("install -Dm755"));

    // Ends with closing brace
    assert!(pkgbuild.trim_end().ends_with('}'));
}

#[test]
fn test_generate_pkgbuild_custom_install_template() {
    let pkgbuild = generate_pkgbuild(&PkgbuildParams {
        name: "mytool",
        version: "1.0.0",
        pkgrel: 1,
        description: "A tool with subdirectory archive",
        url: "https://example.com",
        license: &["MIT".to_string()],
        maintainers: &[],
        contributors: &[],
        depends: &[],
        optdepends: &[],
        conflicts: &[],
        provides: &[],
        replaces: &[],
        backup: &[],
        sources: &[(
            "x86_64".to_string(),
            "https://example.com/mytool.tar.gz".to_string(),
            "hash".to_string(),
        )],
        binary_name: "mytool",
        install_template: Some(
            r#"install -Dm755 "$srcdir/mytool-${pkgver}/mytool" "$pkgdir/usr/bin/mytool""#,
        ),
        extra_install_lines: &[],
        install_file: None,
    })
    .unwrap();

    assert!(pkgbuild.contains("package() {"));
    assert!(
        pkgbuild.contains(
            r#"install -Dm755 "$srcdir/mytool-${pkgver}/mytool" "$pkgdir/usr/bin/mytool""#
        )
    );
    // Should NOT contain the default install line
    assert!(!pkgbuild.contains("\"$srcdir/mytool\" \"$pkgdir/usr/bin/mytool\""));
}

#[test]
fn test_generate_pkgbuild_duplicate_arch_sources() {
    // Regression test: when sources have duplicate architectures, the
    // PKGBUILD should only contain one source per arch.
    let sources = vec![
        (
            "x86_64".to_string(),
            "https://example.com/first-amd64.tar.gz".to_string(),
            "hash1".to_string(),
        ),
        (
            "x86_64".to_string(),
            "https://example.com/second-amd64.tar.gz".to_string(),
            "hash2".to_string(),
        ),
    ];
    // Simulate the deduplication that publish_to_aur does before
    // calling generate_pkgbuild (finding #1).
    let mut seen = std::collections::HashSet::new();
    let deduped: Vec<_> = sources
        .into_iter()
        .filter(|(arch, _, _)| seen.insert(arch.clone()))
        .collect();
    assert_eq!(deduped.len(), 1);
    assert_eq!(deduped[0].1, "https://example.com/first-amd64.tar.gz");
}

// -----------------------------------------------------------------------
// generate_srcinfo tests
// -----------------------------------------------------------------------

#[test]
fn test_generate_srcinfo() {
    let srcinfo = generate_srcinfo(&PkgbuildParams {
            name: "mytool-bin",
            version: "2.5.0",
            pkgrel: 3,
            description: "A fantastic CLI tool",
            url: "https://github.com/org/mytool",
            license: &["Apache-2.0".to_string()],
            maintainers: &["Alice <alice@example.com>".to_string()],
            contributors: &[],
            depends: &["glibc".to_string(), "openssl".to_string()],
            optdepends: &[
                "git: for VCS support".to_string(),
                "bash-completion: for shell completions".to_string(),
            ],
            conflicts: &["mytool-git".to_string()],
            provides: &["mytool".to_string()],
            replaces: &[],
            backup: &[],
            sources: &[
                (
                    "x86_64".to_string(),
                    "https://github.com/org/mytool/releases/download/v2.5.0/mytool-2.5.0-linux-amd64.tar.gz".to_string(),
                    "aabbccdd11223344".to_string(),
                ),
                (
                    "aarch64".to_string(),
                    "https://github.com/org/mytool/releases/download/v2.5.0/mytool-2.5.0-linux-arm64.tar.gz".to_string(),
                    "eeff005566778899".to_string(),
                ),
            ],
            binary_name: "mytool",
            install_template: None,
            extra_install_lines: &[],
            install_file: None,
        }).unwrap();

    // pkgbase line
    assert!(srcinfo.contains("pkgbase = mytool-bin"), "missing pkgbase");

    // pkgver / pkgrel
    assert!(srcinfo.contains("\tpkgver = 2.5.0"), "missing pkgver");
    assert!(srcinfo.contains("\tpkgrel = 3"), "missing pkgrel");

    // description
    assert!(
        srcinfo.contains("\tpkgdesc = A fantastic CLI tool"),
        "missing pkgdesc"
    );

    // url + license
    assert!(
        srcinfo.contains("\turl = https://github.com/org/mytool"),
        "missing url"
    );
    assert!(
        srcinfo.contains("\tlicense = Apache-2.0"),
        "missing license"
    );

    // depends
    assert!(
        srcinfo.contains("\tdepends = glibc"),
        "missing depends glibc"
    );
    assert!(
        srcinfo.contains("\tdepends = openssl"),
        "missing depends openssl"
    );

    // optdepends
    assert!(
        srcinfo.contains("\toptdepends = git: for VCS support"),
        "missing optdepends git"
    );
    assert!(
        srcinfo.contains("\toptdepends = bash-completion: for shell completions"),
        "missing optdepends bash-completion"
    );

    // conflicts
    assert!(
        srcinfo.contains("\tconflicts = mytool-git"),
        "missing conflicts"
    );

    // provides
    assert!(srcinfo.contains("\tprovides = mytool"), "missing provides");

    // arch + source + sha256sums (x86_64)
    assert!(srcinfo.contains("\tarch = x86_64"), "missing arch x86_64");
    assert!(
            srcinfo.contains("\tsource_x86_64 = https://github.com/org/mytool/releases/download/v2.5.0/mytool-2.5.0-linux-amd64.tar.gz"),
            "missing source_x86_64"
        );
    assert!(
        srcinfo.contains("\tsha256sums_x86_64 = aabbccdd11223344"),
        "missing sha256sums_x86_64"
    );

    // arch + source + sha256sums (aarch64)
    assert!(srcinfo.contains("\tarch = aarch64"), "missing arch aarch64");
    assert!(
            srcinfo.contains("\tsource_aarch64 = https://github.com/org/mytool/releases/download/v2.5.0/mytool-2.5.0-linux-arm64.tar.gz"),
            "missing source_aarch64"
        );
    assert!(
        srcinfo.contains("\tsha256sums_aarch64 = eeff005566778899"),
        "missing sha256sums_aarch64"
    );

    // pkgname line at the end
    assert!(
        srcinfo.contains("\npkgname = mytool-bin"),
        "missing pkgname at end"
    );

    // pkgname should appear after the source blocks (i.e. near the end)
    let pkgname_pos = srcinfo.find("pkgname = mytool-bin").unwrap();
    let last_sha_pos = srcinfo.find("sha256sums_aarch64").unwrap();
    assert!(
        pkgname_pos > last_sha_pos,
        "pkgname should appear after source/sha256 blocks"
    );
}

#[test]
fn test_generate_srcinfo_no_optdepends() {
    let srcinfo = generate_srcinfo(&PkgbuildParams {
        name: "simple-bin",
        version: "1.0.0",
        pkgrel: 1,
        description: "Simple tool",
        url: "https://example.com",
        license: &["MIT".to_string()],
        maintainers: &[],
        contributors: &[],
        depends: &[],
        optdepends: &[],
        conflicts: &[],
        provides: &[],
        replaces: &[],
        backup: &[],
        sources: &[(
            "x86_64".to_string(),
            "https://example.com/simple.tar.gz".to_string(),
            "deadbeef".to_string(),
        )],
        binary_name: "simple",
        install_template: None,
        extra_install_lines: &[],
        install_file: None,
    })
    .unwrap();

    // Should not contain optdepends line when empty
    assert!(
        !srcinfo.contains("optdepends"),
        "should not contain optdepends when empty"
    );
    // Should still have basic structure
    assert!(srcinfo.contains("pkgbase = simple-bin"));
    assert!(srcinfo.contains("pkgname = simple-bin"));
}

// -----------------------------------------------------------------------
// publish_to_aur dry-run tests
// -----------------------------------------------------------------------

#[test]
fn test_publish_to_aur_dry_run() {
    use anodizer_core::config::{AurConfig, Config, CrateConfig, PublishConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            aur: Some(AurConfig {
                git_url: Some("ssh://aur@aur.archlinux.org/mytool.git".to_string()),
                description: Some("A great tool".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];

    let ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    let log = StageLogger::new("publish", Verbosity::Normal);

    let pushed = publish_to_aur(&ctx, "mytool", &log).expect("dry-run ok");
    assert!(!pushed, "dry-run must return false (not pushed)");
}

/// Regression: an empty linux-archive set must hard-fail with an
/// actionable error instead of
/// silently writing a PKGBUILD with placeholder URL + empty sha256.
#[test]
fn test_publish_to_aur_empty_linux_archive_set_hard_errors() {
    use anodizer_core::config::{AurConfig, Config, CrateConfig, PublishConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            aur: Some(AurConfig {
                git_url: Some("ssh://aur@aur.archlinux.org/mytool.git".to_string()),
                homepage: Some("https://example.com/mytool".to_string()),
                description: Some("A great tool".to_string()),
                // ids filter that matches nothing forces empty archive set.
                ids: Some(vec!["nonexistent".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];

    // dry_run: false so we reach the archive-set check.
    let ctx = Context::new(config, ContextOptions::default());
    let log = StageLogger::new("publish", Verbosity::Normal);

    let result = publish_to_aur(&ctx, "mytool", &log);
    let err = result.expect_err("empty linux archive set must hard-fail");
    let msg = err.to_string();
    assert!(
        msg.contains("no linux archives matched"),
        "error should explain the no-match condition, got: {msg}"
    );
    assert!(
        msg.contains("nonexistent"),
        "error should cite ids filter, got: {msg}"
    );
    assert!(
        msg.contains("amd64_variant=<default v1>"),
        "unconfigured amd64_variant should carry the default marker, got: {msg}"
    );
}

/// A configured `amd64_variant` prints plainly in the no-match error —
/// no `<default …>` marker that would misattribute an operator choice to
/// a fallback.
#[test]
fn test_publish_to_aur_empty_archive_error_cites_configured_amd64_variant() {
    use anodizer_core::config::{Amd64Variant, AurConfig, Config, CrateConfig, PublishConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            aur: Some(AurConfig {
                git_url: Some("ssh://aur@aur.archlinux.org/mytool.git".to_string()),
                homepage: Some("https://example.com/mytool".to_string()),
                description: Some("A great tool".to_string()),
                ids: Some(vec!["nonexistent".to_string()]),
                amd64_variant: Some(Amd64Variant::V3),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];

    let ctx = Context::new(config, ContextOptions::default());
    let log = StageLogger::new("publish", Verbosity::Normal);

    let err =
        publish_to_aur(&ctx, "mytool", &log).expect_err("empty linux archive set must hard-fail");
    let msg = err.to_string();
    assert!(
        msg.contains("amd64_variant=v3,"),
        "configured amd64_variant should print plainly, got: {msg}"
    );
    assert!(!msg.contains("<default"), "{msg}");
}

#[test]
fn test_publish_to_aur_missing_config() {
    use anodizer_core::config::{Config, CrateConfig, PublishConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig::default()),
        ..Default::default()
    }];

    let ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    let log = StageLogger::new("publish", Verbosity::Normal);

    assert!(publish_to_aur(&ctx, "mytool", &log).is_err());
}

/// `git_url` unset + default name → derives
/// `ssh://aur@aur.archlinux.org/<crate>-bin.git`. An explicit `name:`
/// override (including a template) must produce a matching url. An
/// explicit `git_url` is used verbatim.
#[test]
fn test_aur_resolve_push_git_url_derives_from_name() {
    use anodizer_core::config::{AurConfig, Config};
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    let log = ctx.logger("publish");

    // Unset git_url + default name → `<crate>-bin`.
    let cfg = AurConfig::default();
    assert_eq!(
        aur_resolve_push_git_url(&ctx, &cfg, "mytool", &log).unwrap(),
        "ssh://aur@aur.archlinux.org/mytool-bin.git",
    );

    // Empty-string git_url is treated as unset (still derives).
    let cfg_empty = AurConfig {
        git_url: Some("   ".to_string()),
        ..Default::default()
    };
    assert_eq!(
        aur_resolve_push_git_url(&ctx, &cfg_empty, "mytool", &log).unwrap(),
        "ssh://aur@aur.archlinux.org/mytool-bin.git",
    );

    // Explicit `name:` override → url tracks the overridden package name
    // (no `-bin` suffix forced onto an explicit name).
    let cfg_name = AurConfig {
        name: Some("widget".to_string()),
        ..Default::default()
    };
    assert_eq!(
        aur_resolve_push_git_url(&ctx, &cfg_name, "mytool", &log).unwrap(),
        "ssh://aur@aur.archlinux.org/widget.git",
    );

    // Templated `name:` renders to a matching url.
    let cfg_tmpl = AurConfig {
        name: Some("{{ .ProjectName }}-bin".to_string()),
        ..Default::default()
    };
    // `ProjectName` is empty in a bare default context, so the rendered
    // name is `-bin`; the url must reflect the rendered name exactly.
    assert_eq!(
        aur_resolve_push_git_url(&ctx, &cfg_tmpl, "mytool", &log).unwrap(),
        "ssh://aur@aur.archlinux.org/-bin.git",
    );

    // Explicit git_url is a verbatim override.
    let cfg_override = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/custom.git".to_string()),
        name: Some("widget".to_string()),
        ..Default::default()
    };
    assert_eq!(
        aur_resolve_push_git_url(&ctx, &cfg_override, "mytool", &log).unwrap(),
        "ssh://aur@aur.archlinux.org/custom.git",
    );
}

// -----------------------------------------------------------------------
// Default() resolution
//
// Four defaults are applied at config-load time: name auto-suffixed
// `-bin`, conflicts/provides default to the project name, and pkgrel
// defaults to "1". These tests pin each rule
// against the helper pair (`aur_default_package_name` →
// `aur_resolve_defaults`) so a regression trips a unit test instead of
// slipping through to a malformed PKGBUILD on disk.
// -----------------------------------------------------------------------

/// `aur.name` unset → raw default is `<crate>-bin`. When the crate name
/// already ends in `-bin` (e.g. `foo-bin`), do NOT double-suffix.
#[test]
fn test_aur_default_name_appends_bin_suffix() {
    use anodizer_core::config::AurConfig;

    let cfg = AurConfig::default();

    // Plain crate name → suffix appended.
    assert_eq!(
        aur_default_package_name(&cfg, "mytool"),
        "mytool-bin",
        "name should default to crate_name + '-bin'",
    );

    // Crate name already ends in `-bin` → no double suffix.
    assert_eq!(
        aur_default_package_name(&cfg, "foo-bin"),
        "foo-bin",
        "name must not double-suffix when crate already ends in '-bin'",
    );

    // Explicit `aur.name` wins over the default and is returned verbatim
    // (template rendering is the caller's responsibility).
    let cfg_explicit = AurConfig {
        name: Some("custom-name".to_string()),
        ..Default::default()
    };
    assert_eq!(
        aur_default_package_name(&cfg_explicit, "mytool"),
        "custom-name",
    );
}

/// `aur.conflicts` unset/empty → defaults to `[project_name]`.
/// When `project_name` is empty, falls back to the rendered package name
/// with any trailing `-bin` stripped.
#[test]
fn test_aur_default_conflicts_uses_project_name() {
    use anodizer_core::config::AurConfig;

    // Unset → defaults to [project_name].
    let cfg_unset = AurConfig::default();
    let resolved = aur_resolve_defaults(&cfg_unset, "mytool-bin", "mytool");
    assert_eq!(
        resolved.conflicts,
        vec!["mytool".to_string()],
        "conflicts should default to [project_name] when unset",
    );

    // Empty vec → defaults same as unset.
    let cfg_empty = AurConfig {
        conflicts: Some(vec![]),
        ..Default::default()
    };
    let resolved_empty = aur_resolve_defaults(&cfg_empty, "mytool-bin", "mytool");
    assert_eq!(
        resolved_empty.conflicts,
        vec!["mytool".to_string()],
        "conflicts should default to [project_name] when empty",
    );

    // No project_name → fallback to rendered package name with `-bin` stripped.
    let resolved_no_project = aur_resolve_defaults(&cfg_unset, "mytool-bin", "");
    assert_eq!(
        resolved_no_project.conflicts,
        vec!["mytool".to_string()],
        "conflicts should fall back to stripped package name when project_name empty",
    );

    // Explicit value wins.
    let cfg_explicit = AurConfig {
        conflicts: Some(vec!["other-pkg".to_string()]),
        ..Default::default()
    };
    let resolved_explicit = aur_resolve_defaults(&cfg_explicit, "mytool-bin", "mytool");
    assert_eq!(resolved_explicit.conflicts, vec!["other-pkg".to_string()]);
}

/// `aur.provides` unset/empty → defaults to `[project_name]`.
#[test]
fn test_aur_default_provides_uses_project_name() {
    use anodizer_core::config::AurConfig;

    let cfg_unset = AurConfig::default();
    let resolved = aur_resolve_defaults(&cfg_unset, "mytool-bin", "mytool");
    assert_eq!(
        resolved.provides,
        vec!["mytool".to_string()],
        "provides should default to [project_name] when unset",
    );

    let cfg_empty = AurConfig {
        provides: Some(vec![]),
        ..Default::default()
    };
    let resolved_empty = aur_resolve_defaults(&cfg_empty, "mytool-bin", "mytool");
    assert_eq!(
        resolved_empty.provides,
        vec!["mytool".to_string()],
        "provides should default to [project_name] when empty",
    );

    // Explicit value wins.
    let cfg_explicit = AurConfig {
        provides: Some(vec!["virtual-pkg".to_string()]),
        ..Default::default()
    };
    let resolved_explicit = aur_resolve_defaults(&cfg_explicit, "mytool-bin", "mytool");
    assert_eq!(resolved_explicit.provides, vec!["virtual-pkg".to_string()]);
}

/// `aur.rel` unset → defaults to `1`. Non-numeric or
/// unparseable values also collapse to `1` rather than erroring; explicit
/// numeric values pass through unchanged.
#[test]
fn test_aur_default_pkgrel_is_one() {
    use anodizer_core::config::AurConfig;

    // Unset → 1.
    let cfg_unset = AurConfig::default();
    let resolved = aur_resolve_defaults(&cfg_unset, "mytool-bin", "mytool");
    assert_eq!(resolved.pkgrel, 1, "pkgrel should default to 1 when unset");

    // Explicit numeric value passes through.
    let cfg_explicit = AurConfig {
        rel: Some("3".to_string()),
        ..Default::default()
    };
    let resolved_explicit = aur_resolve_defaults(&cfg_explicit, "mytool-bin", "mytool");
    assert_eq!(resolved_explicit.pkgrel, 3);

    // Non-numeric value falls back to 1 (defensive — the field is a
    // string, so any unparseable input is accepted rather than failing
    // the publish).
    let cfg_bad = AurConfig {
        rel: Some("not-a-number".to_string()),
        ..Default::default()
    };
    let resolved_bad = aur_resolve_defaults(&cfg_bad, "mytool-bin", "mytool");
    assert_eq!(resolved_bad.pkgrel, 1);
}

/// Regression: `base_name` is derived from the *rendered* package name,
/// not the raw template string. Simulates `aur.name = "{{ .ProjectName }}-bin"`
/// rendered against an empty `project_name` (which yields just `"-bin"`).
/// Before the split, the helper carried the unrendered template through
/// to `conflicts`/`provides`, leaking `{{` into the PKGBUILD output.
#[test]
fn test_aur_resolve_defaults_uses_rendered_name_for_base() {
    use anodizer_core::config::AurConfig;

    let cfg = AurConfig::default();
    // `"-bin"` is what `"{{ .ProjectName }}-bin"` renders to when
    // `project_name == ""` — the caller in `publish_to_aur` performs
    // the render *before* invoking `aur_resolve_defaults`.
    let resolved = aur_resolve_defaults(&cfg, "-bin", "");

    assert!(
        !resolved.conflicts[0].contains("{{"),
        "conflicts must not leak unrendered template syntax, got {:?}",
        resolved.conflicts,
    );
    assert_eq!(
        resolved.conflicts[0], "",
        "with rendered name '-bin' and no project_name, base_name strips to ''",
    );
    assert!(
        !resolved.provides[0].contains("{{"),
        "provides must not leak unrendered template syntax, got {:?}",
        resolved.provides,
    );
    assert_eq!(resolved.provides[0], "");
}

/// Regression: `aur.skip_upload: "{{ .IsSnapshot }}"` must template-expand
/// before its bool/auto/empty interpretation. On a snapshot run
/// the rendered value is `"true"` and the publish path must
/// short-circuit to `Ok(())` (no git-push attempt).
#[test]
fn aur_skip_upload_template_expands_to_true_on_snapshot() {
    use anodizer_core::config::{AurConfig, Config, CrateConfig, PublishConfig, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let mut config = Config::default();
    config.project_name = "mytool".to_string();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            aur: Some(AurConfig {
                git_url: Some("ssh://aur@aur.archlinux.org/mytool.git".to_string()),
                description: Some("A great tool".to_string()),
                skip_upload: Some(StringOrBool::String("{{ .IsSnapshot }}".to_string())),
                // ids filter that matches nothing — would normally
                // hard-fail with "no linux archives matched", but the
                // skip_upload short-circuit must run BEFORE the
                // archive check.
                ids: Some(vec!["nonexistent".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            snapshot: true,
            ..Default::default()
        },
    );
    // Populate IsSnapshot template var (normally done by populate_git_vars).
    ctx.template_vars_mut().set("IsSnapshot", "true");

    let log = StageLogger::new("publish", Verbosity::Normal);
    publish_to_aur(&ctx, "mytool", &log).expect(
        "skip_upload='{{ .IsSnapshot }}' on snapshot must short-circuit \
             to Ok(()) before the archive-set check",
    );
}

// -----------------------------------------------------------------------
// quote_pkgdesc
// -----------------------------------------------------------------------

#[test]
fn quote_pkgdesc_plain_uses_double_quotes() {
    assert_eq!(quote_pkgdesc("A great tool"), "\"A great tool\"");
}

#[test]
fn quote_pkgdesc_double_quote_only_switches_to_single() {
    assert_eq!(quote_pkgdesc("Say \"hi\" now"), "'Say \"hi\" now'");
}

#[test]
fn quote_pkgdesc_apostrophe_only_keeps_double() {
    assert_eq!(quote_pkgdesc("don't panic"), "\"don't panic\"");
}

#[test]
fn quote_pkgdesc_both_quotes_escapes_apostrophe() {
    // contains both ' and " -> single-quote wrap with shell-escaped '.
    assert_eq!(quote_pkgdesc("it's \"quoted\""), "'it'\\''s \"quoted\"'");
}

// -----------------------------------------------------------------------
// extract_archive_extension
// -----------------------------------------------------------------------

#[test]
fn extract_archive_extension_compound_tarballs() {
    assert_eq!(
        extract_archive_extension("https://x/a-1.0-linux.tar.gz"),
        "tar.gz"
    );
    assert_eq!(extract_archive_extension("https://x/a.tar.xz"), "tar.xz");
    assert_eq!(extract_archive_extension("https://x/a.tar.zst"), "tar.zst");
}

#[test]
fn extract_archive_extension_simple() {
    assert_eq!(extract_archive_extension("https://x/a.zip"), "zip");
}

#[test]
fn extract_archive_extension_strips_query_and_fragment() {
    assert_eq!(
        extract_archive_extension("https://x/a.tar.gz?token=abc#frag"),
        "tar.gz"
    );
}

#[test]
fn extract_archive_extension_no_extension_yields_empty() {
    assert_eq!(extract_archive_extension("https://x/release/binary"), "");
}

// -----------------------------------------------------------------------
// generate_pkgbuild / generate_srcinfo — empty-extension rename branch
// (the `String::new()` arm of the `if format.is_empty()` in both
// renderers' `rename` construction).
// -----------------------------------------------------------------------

/// A source URL with no archive extension (e.g. a bare binary path)
/// renders a `rename` token with NO trailing `.<ext>` — exercising the
/// `String::new()` arm of `generate_pkgbuild`'s rename builder.
#[test]
fn test_generate_pkgbuild_extensionless_source_has_no_rename_suffix() {
    let pkgbuild = generate_pkgbuild(&PkgbuildParams {
        name: "mytool",
        version: "1.0.0",
        pkgrel: 1,
        description: "A tool",
        url: "https://example.com",
        license: &["MIT".to_string()],
        maintainers: &[],
        contributors: &[],
        depends: &[],
        optdepends: &[],
        conflicts: &[],
        provides: &[],
        replaces: &[],
        backup: &[],
        // Bare binary path — `extract_archive_extension` returns "".
        sources: &[(
            "x86_64".to_string(),
            "https://example.com/download/mytool".to_string(),
            "hash".to_string(),
        )],
        binary_name: "mytool",
        install_template: None,
        extra_install_lines: &[],
        install_file: None,
    })
    .unwrap();

    // rename token is `mytool_${pkgver}_x86_64` with no `.<ext>`.
    assert!(
        pkgbuild.contains(
            "source_x86_64=(\"mytool_${pkgver}_x86_64::https://example.com/download/mytool\")"
        ),
        "extensionless source must render a suffix-free rename:\n{pkgbuild}"
    );
}

/// `.SRCINFO` rename builder mirrors the PKGBUILD one: an extensionless
/// source omits the trailing `.<ext>`. The .SRCINFO template embeds the
/// raw `source_<arch>` URL (not the rename), so the assertion here pins
/// the raw URL survives the empty-extension path without a panic and the
/// arch/source/sha lines render.
#[test]
fn test_generate_srcinfo_extensionless_source_renders() {
    let srcinfo = generate_srcinfo(&PkgbuildParams {
        name: "mytool-bin",
        version: "1.0.0",
        pkgrel: 1,
        description: "A tool",
        url: "https://example.com",
        license: &["MIT".to_string()],
        maintainers: &[],
        contributors: &[],
        depends: &[],
        optdepends: &[],
        conflicts: &[],
        provides: &[],
        replaces: &[],
        backup: &[],
        sources: &[(
            "x86_64".to_string(),
            "https://example.com/download/mytool".to_string(),
            "deadbeef".to_string(),
        )],
        binary_name: "mytool",
        install_template: None,
        extra_install_lines: &[],
        install_file: None,
    })
    .unwrap();

    assert!(srcinfo.contains("\tarch = x86_64"), "{srcinfo}");
    assert!(
        srcinfo.contains("\tsource_x86_64 = https://example.com/download/mytool"),
        "{srcinfo}"
    );
    assert!(
        srcinfo.contains("\tsha256sums_x86_64 = deadbeef"),
        "{srcinfo}"
    );
}

// -----------------------------------------------------------------------
// render_aur_pkgbuild_and_srcinfo_for_crate — the skip-aware render entry
// the offline validator drives. Pure (no git): exercises the url
// fallback / bail, the url_template branch, install-file emission, and the
// skip / if / skip_upload gates.
// -----------------------------------------------------------------------

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{AurConfig, Config, CrateConfig, PublishConfig, StringOrBool};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::log::{StageLogger, Verbosity};

fn render_quiet_log() -> StageLogger {
    StageLogger::new("publish", Verbosity::Quiet)
}

/// A linux amd64 archive carrying url + sha256, matching the AUR filters.
fn linux_amd64_archive(crate_name: &str, url: &str, sha: &str) -> Artifact {
    let mut metadata = std::collections::HashMap::new();
    metadata.insert("url".to_string(), url.to_string());
    metadata.insert("sha256".to_string(), sha.to_string());
    Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("/tmp/{crate_name}-linux-amd64.tar.gz")),
        name: format!("{crate_name}-linux-amd64.tar.gz"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: crate_name.to_string(),
        metadata,
        size: None,
    }
}

/// Build a single-crate context wired to publish `crate_name` to AUR with
/// the supplied `aur` config and one matching linux archive.
fn render_ctx(crate_name: &str, aur: AurConfig, release_github: bool) -> Context {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: crate_name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        release: if release_github {
            Some(anodizer_core::config::ReleaseConfig {
                github: Some(anodizer_core::config::ScmRepoConfig {
                    owner: "myorg".to_string(),
                    name: "mytool".to_string(),
                    token: None,
                }),
                ..Default::default()
            })
        } else {
            None
        },
        publish: Some(PublishConfig {
            aur: Some(aur),
            ..Default::default()
        }),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.artifacts.add(linux_amd64_archive(
        crate_name,
        "https://example.com/mytool-linux-amd64.tar.gz",
        "abc123",
    ));
    ctx
}

/// Build a linux archive for `crate_name` carrying an explicit target
/// triple (so `aur_build_sources` infers the goarch from it) plus a
/// non-empty sha256.
fn linux_archive_for_target(crate_name: &str, target: &str, url: &str, sha: &str) -> Artifact {
    let mut metadata = std::collections::HashMap::new();
    metadata.insert("url".to_string(), url.to_string());
    metadata.insert("sha256".to_string(), sha.to_string());
    Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("/tmp/{crate_name}.tar.gz")),
        name: format!("{crate_name}.tar.gz"),
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata,
        size: None,
    }
}

/// Single-crate: an aarch64 archive must map to the pacman `aarch64`
/// architecture — NOT silently relabeled `x86_64` (the historical
/// `_ => "x86_64"` fallthrough). This is the regression guard for the
/// arch-corruption bug.
#[test]
fn aarch64_archive_maps_to_aarch64_not_x86_64() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        homepage: Some("https://example.com".to_string()),
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            aur: Some(aur),
            ..Default::default()
        }),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.artifacts.add(linux_archive_for_target(
        "mytool",
        "aarch64-unknown-linux-gnu",
        "https://example.com/mytool-linux-arm64.tar.gz",
        "deadbeef",
    ));
    let rendered = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "mytool", &render_quiet_log())
        .expect("render ok")
        .expect("not skipped");
    assert!(
        rendered.pkgbuild.contains("arch=('aarch64')"),
        "aarch64 archive must land under arch=('aarch64'):\n{}",
        rendered.pkgbuild
    );
    assert!(
        rendered.pkgbuild.contains("source_aarch64="),
        "aarch64 archive must emit source_aarch64=:\n{}",
        rendered.pkgbuild
    );
    assert!(
        !rendered.pkgbuild.contains("source_x86_64="),
        "aarch64 archive must NOT be relabeled under x86_64:\n{}",
        rendered.pkgbuild
    );
}

/// An artifact whose architecture has no pacman name (e.g. riscv64) must
/// HARD-FAIL rather than be silently relabeled `x86_64`. The error must
/// name the offending architecture and be actionable.
#[test]
fn unknown_arch_hard_fails_not_relabeled() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        homepage: Some("https://example.com".to_string()),
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            aur: Some(aur),
            ..Default::default()
        }),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.artifacts.add(linux_archive_for_target(
        "mytool",
        "riscv64gc-unknown-linux-gnu",
        "https://example.com/mytool-linux-riscv64.tar.gz",
        "deadbeef",
    ));
    let err = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "mytool", &render_quiet_log())
        .expect_err("unknown arch must hard-fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("riscv64"),
        "error must name the offending arch:\n{msg}"
    );
    assert!(
        msg.contains("pacman") || msg.contains("Arch Linux"),
        "error must explain the pacman arch-naming failure:\n{msg}"
    );
}

/// Dual-licensed crate (`MIT OR Apache-2.0`) renders the pacman
/// `license=()` array with both SPDX ids — matching the zoxide-bin /
/// starship-bin convention — not a single understated id.
#[test]
fn dual_license_renders_license_array() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        homepage: Some("https://example.com".to_string()),
        license: Some("MIT OR Apache-2.0".to_string()),
        ..Default::default()
    };
    let ctx = render_ctx("mytool", aur, false);
    let rendered = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "mytool", &render_quiet_log())
        .expect("render ok")
        .expect("not skipped");
    assert!(
        rendered.pkgbuild.contains("license=('MIT' 'Apache-2.0')"),
        "dual license must render both SPDX ids:\n{}",
        rendered.pkgbuild
    );
    assert!(
        rendered.srcinfo.contains("\tlicense = MIT")
            && rendered.srcinfo.contains("\tlicense = Apache-2.0"),
        ".SRCINFO must emit one license line per id:\n{}",
        rendered.srcinfo
    );
}

/// The default `package()` body installs the LICENSE (REQUIRED), plus man
/// pages and shell completions the archive bundles, gated on existence.
#[test]
fn package_installs_license_man_and_completions() {
    use anodizer_core::config::{ArchiveConfig, ArchivesConfig, CompletionsConfig, ManpagesConfig};
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        homepage: Some("https://example.com".to_string()),
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            completions: Some(CompletionsConfig {
                generate: Some("{{ ArtifactPath }} completions {{ Shell }}".to_string()),
                shells: Some(vec![
                    "bash".to_string(),
                    "zsh".to_string(),
                    "fish".to_string(),
                ]),
                ..Default::default()
            }),
            manpages: Some(ManpagesConfig {
                generate: Some("{{ ArtifactPath }} man".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        publish: Some(PublishConfig {
            aur: Some(aur),
            ..Default::default()
        }),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.artifacts.add(linux_amd64_archive(
        "mytool",
        "https://example.com/mytool-linux-amd64.tar.gz",
        "abc123",
    ));
    let rendered = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "mytool", &render_quiet_log())
        .expect("render ok")
        .expect("not skipped");
    let pkgbuild = &rendered.pkgbuild;
    assert!(
        pkgbuild.contains("usr/share/licenses/$pkgname/"),
        "package() must install LICENSE to /usr/share/licenses/$pkgname/:\n{pkgbuild}"
    );
    assert!(
        pkgbuild.contains("usr/share/man/man1/"),
        "package() must install man pages to /usr/share/man/man1/:\n{pkgbuild}"
    );
    assert!(
        pkgbuild.contains("usr/share/bash-completion/completions/mytool"),
        "package() must install bash completion:\n{pkgbuild}"
    );
    assert!(
        pkgbuild.contains("usr/share/zsh/site-functions/_mytool"),
        "package() must install zsh completion:\n{pkgbuild}"
    );
    assert!(
        pkgbuild.contains("usr/share/fish/vendor_completions.d/mytool.fish"),
        "package() must install fish completion:\n{pkgbuild}"
    );
    // Binary install is still present.
    assert!(
        pkgbuild.contains("install -Dm755 \"$srcdir/mytool\" \"$pkgdir/usr/bin/mytool\""),
        "binary install must remain:\n{pkgbuild}"
    );
}

/// Workspace per-crate: each crate resolves its OWN license/arch with no
/// cross-crate leakage. crate A is dual-licensed amd64-only; crate B is
/// single-license aarch64-only.
#[test]
fn workspace_per_crate_no_license_or_arch_leakage() {
    let aur_a = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/aa-bin.git".to_string()),
        homepage: Some("https://example.com/aa".to_string()),
        license: Some("MIT OR Apache-2.0".to_string()),
        ..Default::default()
    };
    let aur_b = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/bb-bin.git".to_string()),
        homepage: Some("https://example.com/bb".to_string()),
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    let mut config = Config::default();
    config.crates = vec![
        CrateConfig {
            name: "aa".to_string(),
            path: "aa".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                aur: Some(aur_a),
                ..Default::default()
            }),
            ..Default::default()
        },
        CrateConfig {
            name: "bb".to_string(),
            path: "bb".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                aur: Some(aur_b),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.artifacts.add(linux_archive_for_target(
        "aa",
        "x86_64-unknown-linux-gnu",
        "https://example.com/aa-linux-amd64.tar.gz",
        "aaaa",
    ));
    ctx.artifacts.add(linux_archive_for_target(
        "bb",
        "aarch64-unknown-linux-gnu",
        "https://example.com/bb-linux-arm64.tar.gz",
        "bbbb",
    ));

    let ra = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "aa", &render_quiet_log())
        .expect("render aa")
        .expect("aa not skipped");
    let rb = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "bb", &render_quiet_log())
        .expect("render bb")
        .expect("bb not skipped");

    // aa: dual license, x86_64 only.
    assert!(
        ra.pkgbuild.contains("license=('MIT' 'Apache-2.0')"),
        "aa must carry its own dual license:\n{}",
        ra.pkgbuild
    );
    assert!(
        ra.pkgbuild.contains("arch=('x86_64')"),
        "aa must be x86_64 only:\n{}",
        ra.pkgbuild
    );
    assert!(
        !ra.pkgbuild.contains("aarch64"),
        "aa must not leak bb's aarch64:\n{}",
        ra.pkgbuild
    );
    // bb: single license, aarch64 only.
    assert!(
        rb.pkgbuild.contains("license=('MIT')"),
        "bb must carry its own single license:\n{}",
        rb.pkgbuild
    );
    assert!(
        rb.pkgbuild.contains("arch=('aarch64')"),
        "bb must be aarch64 only:\n{}",
        rb.pkgbuild
    );
    assert!(
        !rb.pkgbuild.contains("Apache-2.0"),
        "bb must not leak aa's Apache-2.0:\n{}",
        rb.pkgbuild
    );
}

/// With no `homepage`/`metadata.homepage` but a `release.github`
/// owner/name, the PKGBUILD `url=` falls back to the derived GitHub repo
/// URL (the `else if let Some(gh)` arm of the url resolver).
#[test]
fn render_url_falls_back_to_release_github() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    let ctx = render_ctx("mytool", aur, true);
    let rendered = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "mytool", &render_quiet_log())
        .expect("render ok")
        .expect("not skipped");
    assert!(
        rendered
            .pkgbuild
            .contains("url=\"https://github.com/myorg/mytool\""),
        "url must derive from release.github when homepage unset:\n{}",
        rendered.pkgbuild
    );
}

/// No `homepage` AND no `release.github` is a hard error: the PKGBUILD
/// `url=` cannot be resolved, so the render bails naming the crate and
/// pointing at `homepage` / `release.github`.
#[test]
fn render_url_unresolvable_bails_with_actionable_error() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    let ctx = render_ctx("mytool", aur, false);
    let err = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "mytool", &render_quiet_log())
        .expect_err("no url source must bail");
    let msg = format!("{err:#}");
    assert!(msg.contains("no url configured"), "{msg}");
    assert!(msg.contains("mytool"), "{msg}");
    assert!(msg.contains("publish.aur.homepage"), "{msg}");
}

/// `url_template` overrides the artifact's `metadata.url`: the rendered
/// `source_<arch>=` line carries the templated URL (os/arch/version
/// substituted), not the original archive URL.
#[test]
fn render_url_template_drives_source_url() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        homepage: Some("https://example.com".to_string()),
        license: Some("MIT".to_string()),
        url_template: Some("https://dl/{{ .Version }}/{{ .Os }}-{{ .Arch }}.tar.gz".to_string()),
        ..Default::default()
    };
    let mut ctx = render_ctx("mytool", aur, false);
    // The url_template interpolates `{{ .Version }}`; without a resolved
    // `Version` var the rendered URL has an empty version segment and the
    // PKGBUILD's `version → ${pkgver}` substitution has nothing to replace.
    // Set it the way the live publish path does (the `Version` template var).
    ctx.template_vars_mut().set("Version", "1.0.0");
    let rendered = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "mytool", &render_quiet_log())
        .expect("render ok")
        .expect("not skipped");
    // The substituted-version URL replaces the literal version with
    // ${pkgver} in the PKGBUILD; arch=x86_64 / os=linux from the template.
    assert!(
        rendered
            .pkgbuild
            .contains("https://dl/${pkgver}/linux-x86_64.tar.gz"),
        "url_template must drive the source URL:\n{}",
        rendered.pkgbuild
    );
    assert!(
        !rendered
            .pkgbuild
            .contains("example.com/mytool-linux-amd64.tar.gz"),
        "the original metadata.url must not appear when url_template is set:\n{}",
        rendered.pkgbuild
    );
}

/// `url_template` with `{{ .ArtifactName }}` must resolve to the archive
/// filename (e.g. `mytool-linux-amd64.tar.gz`), not the crate name
/// (`mytool`). The crate name has no extension, so `ArtifactName` was
/// never set under the old code path — the template rendered as the
/// literal `{{ .ArtifactName }}` instead of the real filename.
#[test]
fn url_template_artifact_name_resolves_to_archive_filename() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        homepage: Some("https://example.com".to_string()),
        license: Some("MIT".to_string()),
        url_template: Some("https://dl/v{{ .Version }}/{{ .ArtifactName }}".to_string()),
        ..Default::default()
    };
    let mut ctx = render_ctx("mytool", aur, false);
    ctx.template_vars_mut().set("Version", "1.0.0");
    let rendered = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "mytool", &render_quiet_log())
        .expect("render ok")
        .expect("not skipped");
    assert!(
        rendered
            .pkgbuild
            .contains("https://dl/v${pkgver}/mytool-linux-amd64.tar.gz"),
        "ArtifactName must resolve to archive filename, not crate name:\n{}",
        rendered.pkgbuild
    );
    assert!(
        !rendered.pkgbuild.contains("ArtifactName"),
        "literal ArtifactName template must not appear in output:\n{}",
        rendered.pkgbuild
    );
}

/// `install:` content makes the rendered PKGBUILD carry an
/// `install=<base>.install` line where `<base>` is the package name with a
/// trailing `-bin` stripped (the `install_file_ref = Some(...)` branch of
/// `render_aur_inner`).
#[test]
fn render_with_install_emits_install_line() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        homepage: Some("https://example.com".to_string()),
        license: Some("MIT".to_string()),
        install: Some("post_install() { echo hi; }".to_string()),
        ..Default::default()
    };
    let ctx = render_ctx("mytool", aur, false);
    let rendered = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "mytool", &render_quiet_log())
        .expect("render ok")
        .expect("not skipped");
    // Default name `mytool-bin` → install base strips to `mytool`.
    assert!(
        rendered.pkgbuild.contains("install=mytool.install"),
        "install= line must reference <base>.install:\n{}",
        rendered.pkgbuild
    );
}

/// A truthy `skip` short-circuits the render to `Ok(None)` (the crate is
/// suppressed entirely) — no PKGBUILD is produced.
#[test]
fn render_skip_true_returns_none() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        homepage: Some("https://example.com".to_string()),
        license: Some("MIT".to_string()),
        skip: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let ctx = render_ctx("mytool", aur, false);
    let out = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "mytool", &render_quiet_log())
        .expect("render ok");
    assert!(out.is_none(), "skip=true must render None");
}

/// A falsy `if:` condition skips the crate (the `if` gate returns
/// `Ok(None)` before any render work).
#[test]
fn render_if_false_returns_none() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        homepage: Some("https://example.com".to_string()),
        license: Some("MIT".to_string()),
        if_condition: Some("false".to_string()),
        ..Default::default()
    };
    let ctx = render_ctx("mytool", aur, false);
    let out = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "mytool", &render_quiet_log())
        .expect("render ok");
    assert!(out.is_none(), "if:false must render None");
}

/// A truthy `skip_upload` skips the render (the upload gate fires before
/// the inner render, returning `Ok(None)`).
#[test]
fn render_skip_upload_true_returns_none() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        homepage: Some("https://example.com".to_string()),
        license: Some("MIT".to_string()),
        skip_upload: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let ctx = render_ctx("mytool", aur, false);
    let out = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "mytool", &render_quiet_log())
        .expect("render ok");
    assert!(out.is_none(), "skip_upload=true must render None");
}

/// A templated `aur.homepage` (`{{ .Tag }}`) is template-rendered into the
/// PKGBUILD `url=` line and the .SRCINFO `url =` line — the literal
/// delimiters must NOT leak. Regression for the raw-emit url bug.
#[test]
fn render_homepage_template_is_rendered_into_url() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        homepage: Some("https://example.com/releases/{{ .Tag }}".to_string()),
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    let mut ctx = render_ctx("mytool", aur, false);
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    let rendered = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "mytool", &render_quiet_log())
        .expect("render ok")
        .expect("not skipped");
    assert!(
        rendered
            .pkgbuild
            .contains("url=\"https://example.com/releases/v1.2.3\""),
        "templated homepage must render into PKGBUILD url=:\n{}",
        rendered.pkgbuild
    );
    assert!(
        !rendered.pkgbuild.contains("{{"),
        "PKGBUILD must carry no unrendered `{{{{`:\n{}",
        rendered.pkgbuild
    );
    assert!(
        rendered
            .srcinfo
            .contains("url = https://example.com/releases/v1.2.3"),
        ".SRCINFO url must carry the resolved value:\n{}",
        rendered.srcinfo
    );
    assert!(
        !rendered.srcinfo.contains("{{"),
        ".SRCINFO must carry no unrendered `{{{{`:\n{}",
        rendered.srcinfo
    );
}

/// `render_aur_pkgbuild_and_srcinfo_for_crate` errors when the crate has
/// no `aur` block at all (the `ok_or_else` on the missing config).
#[test]
fn render_missing_aur_block_errors() {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig::default()),
        ..Default::default()
    }];
    let ctx = Context::new(config, ContextOptions::default());
    let err = render_aur_pkgbuild_and_srcinfo_for_crate(&ctx, "mytool", &render_quiet_log())
        .expect_err("missing aur block must error");
    assert!(
        format!("{err:#}").contains("no aur config"),
        "error must name the missing aur config: {err:#}"
    );
}

/// `crate_has_aur_linux_archive` returns `Ok(true)` when a matching linux
/// archive exists and `Ok(false)` (true absence) when none matches the
/// `ids:` filter — the validator's skip-vs-error discriminator.
#[test]
fn crate_has_aur_linux_archive_true_and_false() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        ..Default::default()
    };
    let ctx = render_ctx("mytool", aur.clone(), false);
    assert!(
        crate_has_aur_linux_archive(&ctx, &aur, "mytool").expect("ok"),
        "a matching linux archive must report present",
    );

    // ids filter matching nothing → clean absence (Ok(false), not Err).
    let aur_no_match = AurConfig {
        ids: Some(vec!["nonexistent".to_string()]),
        ..aur
    };
    assert!(
        !crate_has_aur_linux_archive(&ctx, &aur_no_match, "mytool").expect("ok"),
        "no archive matching ids must report absent",
    );
}

/// `resolve_aur_credentials_from_config` skips crates with no
/// `publish.aur` block (the `continue` on the let-else) and returns
/// `(None, None)` for an unknown git_url.
#[test]
fn resolve_credentials_skips_non_aur_crates() {
    let mut config = Config::default();
    config.crates = vec![
        // No publish.aur — must be skipped by the let-else continue.
        CrateConfig {
            name: "plain".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig::default()),
            ..Default::default()
        },
        CrateConfig {
            name: "withkey".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                aur: Some(AurConfig {
                    git_url: Some("ssh://aur@aur.archlinux.org/withkey.git".to_string()),
                    private_key: Some("KEYBYTES".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];
    let ctx = Context::new(config, ContextOptions::default());
    let (pk, _) =
        resolve_aur_credentials_from_config(&ctx, "ssh://aur@aur.archlinux.org/withkey.git")
            .unwrap();
    assert_eq!(pk.as_deref(), Some("KEYBYTES"));
    // Unknown url → (None, None) after walking past the skipped crate.
    let (pk_none, ssh_none) =
        resolve_aur_credentials_from_config(&ctx, "ssh://aur@x/y.git").unwrap();
    assert!(pk_none.is_none() && ssh_none.is_none());
}

/// The publisher skips on nightly (its `skips_on_nightly` is `true`).
#[test]
fn aur_publisher_skips_on_nightly() {
    use anodizer_core::Publisher;
    assert!(AurOurPublisher::new().skips_on_nightly());
}

/// A truthy `skip` short-circuits `publish_to_aur` to `Ok(false)` via
/// `aur_check_skip_and_resolve_git_url`'s skip branch — before any archive
/// build or git clone (the `ids` filter that would otherwise hard-fail is
/// never reached).
#[test]
fn publish_to_aur_skip_true_returns_false_before_archive_check() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        homepage: Some("https://example.com".to_string()),
        license: Some("MIT".to_string()),
        skip: Some(StringOrBool::Bool(true)),
        // Would hard-fail "no linux archives matched" if the skip gate
        // did not short-circuit first.
        ids: Some(vec!["nonexistent".to_string()]),
        ..Default::default()
    };
    let ctx = render_ctx("mytool", aur, false);
    let pushed = publish_to_aur(&ctx, "mytool", &render_quiet_log()).expect("skip ok");
    assert!(!pushed, "skip=true must short-circuit to Ok(false)");
}

/// A falsy `if:` condition short-circuits `publish_to_aur` to `Ok(false)`
/// (the `if` gate in `aur_check_skip_and_resolve_git_url`).
#[test]
fn publish_to_aur_if_false_returns_false() {
    let aur = AurConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
        homepage: Some("https://example.com".to_string()),
        license: Some("MIT".to_string()),
        if_condition: Some("false".to_string()),
        ids: Some(vec!["nonexistent".to_string()]),
        ..Default::default()
    };
    let ctx = render_ctx("mytool", aur, false);
    let pushed = publish_to_aur(&ctx, "mytool", &render_quiet_log()).expect("if:false ok");
    assert!(!pushed, "if:false must short-circuit to Ok(false)");
}

// -----------------------------------------------------------------------
// Live git-over-ssh publish path — clone, write PKGBUILD/.SRCINFO/.install,
// commit, push to AUR `master`. Driven against a local bare repo (the
// `clone_repo_with_auth` no-token branch is a plain `git clone <localpath>`).
// `#[cfg(unix)]`-gated: spawns git, sets process env for commit identity,
// and asserts unix-path file modes. Precedent: homebrew/publish_formula.rs
// `make_bare_tap`.
// -----------------------------------------------------------------------

#[cfg(unix)]
fn git_ok(dir: &std::path::Path, args: &[&str]) {
    anodizer_core::test_helpers::git_test_ok(dir, args)
}

#[cfg(unix)]
fn git_stdout(dir: &std::path::Path, args: &[&str]) -> String {
    anodizer_core::test_helpers::git_test_stdout(dir, args)
}

/// A bare AUR repo seeded with one commit on `master`. Returns its
/// filesystem path (a usable local clone URL) plus the holder tempdir.
#[cfg(unix)]
fn make_bare_aur_repo() -> (String, tempfile::TempDir) {
    let bare = tempfile::tempdir().expect("bare tempdir");
    let seed = tempfile::tempdir().expect("seed tempdir");
    git_ok(bare.path(), &["init", "--bare", "-b", "master"]);
    git_ok(seed.path(), &["init", "-b", "master"]);
    git_ok(seed.path(), &["config", "user.email", "t@example.invalid"]);
    git_ok(seed.path(), &["config", "user.name", "T"]);
    git_ok(seed.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(seed.path().join("README"), "aur\n").unwrap();
    git_ok(seed.path(), &["add", "README"]);
    git_ok(seed.path(), &["commit", "-m", "seed"]);
    assert!(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.args(["remote", "add", "origin"])
                    .arg(bare.path())
                    .current_dir(seed.path());
                cmd
            },
            "git",
        )
        .status
        .success(),
        "git remote add failed"
    );
    git_ok(seed.path(), &["push", "-u", "origin", "master"]);
    (bare.path().to_string_lossy().into_owned(), bare)
}

/// Read a file as it landed on the bare repo's `master` ref.
#[cfg(unix)]
fn aur_show(bare: &std::path::Path, path: &str) -> String {
    git_stdout(bare, &["show", &format!("master:{path}")])
}

/// Build a single-crate context whose `aur.git_url` points the clone at a
/// local bare repo, with one matching linux archive registered.
#[cfg(unix)]
fn live_ctx(bare_url: &str, install: Option<&str>) -> Context {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            aur: Some(AurConfig {
                git_url: Some(bare_url.to_string()),
                homepage: Some("https://example.com/mytool".to_string()),
                license: Some("MIT".to_string()),
                description: Some("A great tool".to_string()),
                install: install.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.artifacts.add(linux_amd64_archive(
        "mytool",
        "https://example.com/mytool-1.2.3-linux-amd64.tar.gz",
        "abc123",
    ));
    ctx
}

/// End-to-end: `publish_to_aur` clones the bare repo, writes
/// PKGBUILD/.SRCINFO, commits, and pushes to `master`. Assert the pushed
/// bytes (PKGBUILD pkgname + .SRCINFO pkgbase) and the `true` push outcome.
#[cfg(unix)]
#[test]
fn publish_to_aur_pushes_pkgbuild_and_srcinfo_to_master() {
    let (bare_url, bare) = make_bare_aur_repo();
    let ctx = live_ctx(&bare_url, None);
    let log = render_quiet_log();

    let pushed = publish_to_aur(&ctx, "mytool", &log).expect("publish ok");
    assert!(pushed, "a fresh PKGBUILD must report a push");

    let pkgbuild = aur_show(std::path::Path::new(&bare_url), "PKGBUILD");
    assert!(pkgbuild.contains("pkgname='mytool-bin'"), "{pkgbuild}");
    assert!(
        pkgbuild.contains("url=\"https://example.com/mytool\""),
        "{pkgbuild}"
    );
    let srcinfo = aur_show(std::path::Path::new(&bare_url), ".SRCINFO");
    assert!(srcinfo.contains("pkgbase = mytool-bin"), "{srcinfo}");
    assert!(srcinfo.contains("pkgname = mytool-bin"), "{srcinfo}");
    drop(bare);
}

/// A second `publish_to_aur` against an already-current repo pushes
/// nothing new: `commit_and_push_with_opts` reports `NoChanges`, so the
/// publisher returns `false`.
#[cfg(unix)]
#[test]
fn publish_to_aur_second_run_no_changes_returns_false() {
    let (bare_url, bare) = make_bare_aur_repo();
    let ctx = live_ctx(&bare_url, None);
    let log = render_quiet_log();

    assert!(
        publish_to_aur(&ctx, "mytool", &log).expect("first publish ok"),
        "first publish must push"
    );
    assert!(
        !publish_to_aur(&ctx, "mytool", &log).expect("second publish ok"),
        "an unchanged repo must report no push (NoChanges)"
    );
    drop(bare);
}

/// With `install:` set, the `.install` file lands on `master` alongside
/// PKGBUILD/.SRCINFO and the PKGBUILD references it.
#[cfg(unix)]
#[test]
fn publish_to_aur_writes_install_file() {
    let (bare_url, bare) = make_bare_aur_repo();
    let ctx = live_ctx(&bare_url, Some("post_install() { echo hi; }"));
    let log = render_quiet_log();

    assert!(publish_to_aur(&ctx, "mytool", &log).expect("publish ok"));
    let pkgbuild = aur_show(std::path::Path::new(&bare_url), "PKGBUILD");
    assert!(pkgbuild.contains("install=mytool.install"), "{pkgbuild}");
    let install = aur_show(std::path::Path::new(&bare_url), "mytool.install");
    assert_eq!(install, "post_install() { echo hi; }");
    drop(bare);
}

/// `directory:` renders a subdirectory inside the cloned repo and the
/// PKGBUILD lands under it (the `aur_resolve_output_dir` create-subdir
/// branch).
#[cfg(unix)]
#[test]
fn publish_to_aur_directory_nests_output() {
    let (bare_url, bare) = make_bare_aur_repo();
    let mut ctx = live_ctx(&bare_url, None);
    if let Some(p) = ctx.config.crates[0].publish.as_mut()
        && let Some(a) = p.aur.as_mut()
    {
        a.directory = Some("packages/mytool".to_string());
    }
    let log = render_quiet_log();
    assert!(publish_to_aur(&ctx, "mytool", &log).expect("publish ok"));
    let pkgbuild = aur_show(std::path::Path::new(&bare_url), "packages/mytool/PKGBUILD");
    assert!(pkgbuild.contains("pkgname='mytool-bin'"), "{pkgbuild}");
    drop(bare);
}

/// Cloning a path that is not a git repo fails: `publish_to_aur`
/// propagates the clone error naming the `aur` publisher (the repo was
/// never touched).
#[cfg(unix)]
#[test]
fn publish_to_aur_clone_failure_errors() {
    let bogus = tempfile::tempdir().expect("bogus dir");
    let bogus_url = bogus.path().to_string_lossy().into_owned();
    let ctx = live_ctx(&bogus_url, None);
    let log = render_quiet_log();
    let err = publish_to_aur(&ctx, "mytool", &log).expect_err("cloning a non-repo path must fail");
    assert!(
        format!("{err:#}").contains("aur"),
        "error must name the publisher: {err:#}"
    );
    drop(bogus);
}

/// Full `Publisher::run` over a configured crate pushes the package and
/// records exactly one rollback target carrying the pushed git_url;
/// `rollback` then git-reverts that target without error.
#[cfg(unix)]
#[test]
fn aur_publisher_run_pushes_and_rollback_reverts() {
    use anodizer_core::Publisher;
    let (bare_url, bare) = make_bare_aur_repo();
    // Point project_root at a hermetic `v0.1.0`-tagged repo so the per-crate
    // scope resolves "mytool"'s tag (`v{{ .Version }}`) deterministically
    // rather than from the process cwd's tags, which a checkout with no
    // fetched tags (CI) leaves empty — starving the resolution.
    let scope_repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = live_ctx(&bare_url, None);
    ctx.options.project_root = Some(scope_repo.path().to_path_buf());
    ctx.options.selected_crates = vec!["mytool".to_string()];
    let p = AurOurPublisher::new();

    let evidence = p.run(&mut ctx).expect("run ok");
    let targets = decode_aur_our_targets(&evidence.extra);
    assert_eq!(targets.len(), 1, "one push → one rollback target");
    assert_eq!(targets[0].git_url, bare_url);
    assert_eq!(targets[0].target, "mytool-bin");

    // A commit landed on master before rollback.
    let before = git_stdout(
        std::path::Path::new(&bare_url),
        &["rev-list", "--count", "master"],
    );
    // Rollback git-reverts the AUR repo; it must not error.
    p.rollback(&mut ctx, &evidence).expect("rollback ok");
    let after = git_stdout(
        std::path::Path::new(&bare_url),
        &["rev-list", "--count", "master"],
    );
    assert!(
        after.parse::<u32>().unwrap() > before.parse::<u32>().unwrap(),
        "rollback must add a revert commit (before={before}, after={after})"
    );
    drop(bare);
}

/// A non-empty `git_ssh_command` routes the clone through `aur_clone_repo`'s
/// SSH branch (`clone_repo_ssh`) instead of `clone_repo_with_auth`. For a
/// local-path clone git ignores `GIT_SSH_COMMAND`, so the clone still
/// succeeds and the package pushes — exercising the SSH-branch + post-clone
/// `core.sshCommand` config-write that the no-key path never touches.
#[cfg(unix)]
#[test]
fn publish_to_aur_ssh_command_routes_through_ssh_clone() {
    let (bare_url, bare) = make_bare_aur_repo();
    let mut ctx = live_ctx(&bare_url, None);
    if let Some(p) = ctx.config.crates[0].publish.as_mut()
        && let Some(a) = p.aur.as_mut()
    {
        a.git_ssh_command = Some("ssh -o StrictHostKeyChecking=no".to_string());
    }
    let log = render_quiet_log();
    assert!(
        publish_to_aur(&ctx, "mytool", &log).expect("ssh-branch publish ok"),
        "SSH-branch clone of a local bare repo must still push"
    );
    let pkgbuild = aur_show(std::path::Path::new(&bare_url), "PKGBUILD");
    assert!(pkgbuild.contains("pkgname='mytool-bin'"), "{pkgbuild}");
    drop(bare);
}

/// A templated `aur.private_key` (`{{ .Env.AUR_SSH_KEY }}`) must be
/// rendered to the env value before `aur_clone_repo` writes it to the
/// SSH key file — the literal `{{` reaching the key file is the
/// canonical `error in libcrypto` failure. The clone target is the local
/// bare repo (git ignores `GIT_SSH_COMMAND` for a filesystem path), so the
/// clone succeeds and the key persisted in `.git/` is inspectable.
#[cfg(unix)]
#[test]
fn aur_clone_repo_renders_templated_private_key_before_write() {
    let (bare_url, bare) = make_bare_aur_repo();
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut()
        .set_env("AUR_SSH_KEY", "RENDERED-AUR-KEY\n");
    let aur_cfg = AurConfig {
        private_key: Some("{{ .Env.AUR_SSH_KEY }}".to_string()),
        ..Default::default()
    };
    let log = render_quiet_log();
    let parent = tempfile::tempdir().expect("parent");
    let dest = parent.path().join("clone-target");
    aur_clone_repo(&ctx, &aur_cfg, &bare_url, &dest, &log).expect("ssh clone ok");
    let key_path = dest.join(".git").join("anodizer_ssh_key");
    let body = std::fs::read_to_string(&key_path).expect("persisted key written");
    assert_eq!(
        body, "RENDERED-AUR-KEY\n",
        "private_key must be the rendered env value, never the literal template"
    );
    assert!(
        !body.contains("{{"),
        "the literal template must never reach the SSH key file"
    );
    drop(bare);
}

/// The rollback credential resolver renders a templated `private_key` /
/// `git_ssh_command` so the recorded revert target carries the resolved
/// secret, not the literal `{{ .Env.X }}` that would fail ssh at revert.
#[test]
fn resolve_aur_credentials_renders_templates() {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            aur: Some(AurConfig {
                git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
                private_key: Some("{{ .Env.AUR_SSH_KEY }}".to_string()),
                git_ssh_command: Some("ssh -i {{ .Env.KEYFILE }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set_env("AUR_SSH_KEY", "REAL-KEY");
    ctx.template_vars_mut().set_env("KEYFILE", "/run/k");
    let (pk, ssh) =
        resolve_aur_credentials_from_config(&ctx, "ssh://aur@aur.archlinux.org/mytool-bin.git")
            .unwrap();
    assert_eq!(pk.as_deref(), Some("REAL-KEY"));
    assert_eq!(ssh.as_deref(), Some("ssh -i /run/k"));
    assert!(!pk.unwrap().contains("{{"));
}
