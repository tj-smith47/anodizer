use super::*;

use anodizer_core::config::AurSourceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

use crate::util;

#[test]
fn test_generate_source_pkgbuild() {
    let maintainers = vec!["Test <test@example.com>".to_string()];
    let depends = vec!["openssl".to_string()];
    let makedepends = vec!["rust".to_string(), "cargo".to_string()];
    let conflicts = vec!["myapp-bin".to_string()];
    let provides = vec!["myapp".to_string()];
    let meta = AurMeta {
        name: "myapp",
        version: "1.0.0",
        pkgrel: 1,
        description: "A test application",
        homepage: "https://example.com",
        license: &["MIT".to_string()],
        arches: &["x86_64".to_string(), "aarch64".to_string()],
    };
    let deps = AurDeps {
        depends: &depends,
        makedepends: &makedepends,
        optdepends: &[],
        conflicts: &conflicts,
        provides: &provides,
    };
    let extras = AurExtras {
        people: AurPeople {
            maintainers: &maintainers,
            contributors: &[],
        },
        hooks: AurHooks {
            prepare: None,
            build: None,
            package: None,
        },
        backup: &[],
        binary_name: "myapp",
        install_file: None,
    };
    let pkgbuild = generate_source_pkgbuild(
        &meta,
        &deps,
        &extras,
        "https://github.com/user/myapp/archive/refs/tags/v1.0.0.tar.gz",
    );

    assert!(pkgbuild.contains("pkgname='myapp'"));
    assert!(pkgbuild.contains("pkgver='1.0.0'"));
    assert!(pkgbuild.contains("pkgrel=1"));
    assert!(pkgbuild.contains("arch=('x86_64' 'aarch64')"));
    assert!(pkgbuild.contains("makedepends=('rust' 'cargo')"));
    assert!(pkgbuild.contains("conflicts=('myapp-bin')"));
    assert!(pkgbuild.contains("cargo build --release --locked"));
    assert!(pkgbuild.contains("install -Dm755"));
    assert!(pkgbuild.contains("# Maintainer: Test <test@example.com>"));
}

#[test]
fn test_generate_source_pkgbuild_custom_build() {
    let meta = AurMeta {
        name: "myapp",
        version: "1.0.0",
        pkgrel: 1,
        description: "Test",
        homepage: "",
        license: &["MIT".to_string()],
        arches: &["x86_64".to_string(), "aarch64".to_string()],
    };
    let deps = AurDeps {
        depends: &[],
        makedepends: &[],
        optdepends: &[],
        conflicts: &[],
        provides: &[],
    };
    let extras = AurExtras {
        people: AurPeople {
            maintainers: &[],
            contributors: &[],
        },
        hooks: AurHooks {
            prepare: Some("cd myapp\npatch -p1 < fix.patch"),
            build: Some("make"),
            package: Some("make install DESTDIR=\"$pkgdir\""),
        },
        backup: &[],
        binary_name: "myapp",
        install_file: None,
    };
    let pkgbuild =
        generate_source_pkgbuild(&meta, &deps, &extras, "https://example.com/source.tar.gz");

    assert!(pkgbuild.contains("prepare() {"));
    assert!(pkgbuild.contains("patch -p1 < fix.patch"));
    assert!(pkgbuild.contains("make\n}"));
    assert!(pkgbuild.contains("make install DESTDIR=\"$pkgdir\""));
}

#[test]
fn test_generate_source_pkgbuild_install_file() {
    let meta = AurMeta {
        name: "myapp",
        version: "1.0.0",
        pkgrel: 1,
        description: "Test",
        homepage: "",
        license: &["MIT".to_string()],
        arches: &["x86_64".to_string(), "aarch64".to_string()],
    };
    let deps = AurDeps {
        depends: &[],
        makedepends: &[],
        optdepends: &[],
        conflicts: &[],
        provides: &[],
    };
    // install=<name>.install only emitted when install_file is Some.
    let with = AurExtras {
        people: AurPeople {
            maintainers: &[],
            contributors: &[],
        },
        hooks: AurHooks {
            prepare: None,
            build: None,
            package: None,
        },
        backup: &[],
        binary_name: "myapp",
        install_file: Some("myapp.install"),
    };
    let pkgbuild =
        generate_source_pkgbuild(&meta, &deps, &with, "https://example.com/source.tar.gz");
    assert!(
        pkgbuild.contains("install=myapp.install"),
        "PKGBUILD must emit install=<name>.install when set:\n{pkgbuild}"
    );

    let without = AurExtras {
        install_file: None,
        ..with
    };
    let pkgbuild_none =
        generate_source_pkgbuild(&meta, &deps, &without, "https://example.com/source.tar.gz");
    assert!(
        !pkgbuild_none.contains("install="),
        "PKGBUILD must NOT emit install= when unset:\n{pkgbuild_none}"
    );
}

#[test]
fn test_write_aur_source_files_writes_install() {
    let dir = tempfile::tempdir().unwrap();
    // With install content: the .install file is written.
    write_aur_source_files(
        dir.path(),
        "PKGBUILD-body",
        "SRCINFO-body",
        "myapp.install",
        Some("post_install() { echo hi; }"),
        "aur_source",
    )
    .unwrap();
    assert!(dir.path().join("PKGBUILD").exists());
    assert!(dir.path().join(".SRCINFO").exists());
    let install_path = dir.path().join("myapp.install");
    assert!(
        install_path.exists(),
        ".install file must be written when content is set"
    );
    assert_eq!(
        std::fs::read_to_string(&install_path).unwrap(),
        "post_install() { echo hi; }"
    );

    // Without install content: no .install file appears.
    let dir2 = tempfile::tempdir().unwrap();
    write_aur_source_files(
        dir2.path(),
        "PKGBUILD-body",
        "SRCINFO-body",
        "myapp.install",
        None,
        "aur_source",
    )
    .unwrap();
    assert!(
        !dir2.path().join("myapp.install").exists(),
        ".install file must NOT be written when content is unset"
    );
}

#[test]
fn test_generate_source_srcinfo() {
    let depends = vec!["openssl".to_string()];
    let makedepends = vec!["rust".to_string(), "cargo".to_string()];
    let conflicts = vec!["myapp-bin".to_string()];
    let provides = vec!["myapp".to_string()];
    let meta = AurMeta {
        name: "myapp",
        version: "1.0.0",
        pkgrel: 1,
        description: "A test application",
        homepage: "https://example.com",
        license: &["MIT".to_string()],
        arches: &["x86_64".to_string(), "aarch64".to_string()],
    };
    let deps = AurDeps {
        depends: &depends,
        makedepends: &makedepends,
        optdepends: &[],
        conflicts: &conflicts,
        provides: &provides,
    };
    let srcinfo = generate_source_srcinfo(
        &meta,
        &deps,
        "https://github.com/user/myapp/archive/refs/tags/v1.0.0.tar.gz",
    );

    assert!(srcinfo.contains("pkgbase = myapp"));
    assert!(srcinfo.contains("\tpkgver = 1.0.0"));
    assert!(srcinfo.contains("\tmakedepends = rust"));
    assert!(srcinfo.contains("\tdepends = openssl"));
    assert!(srcinfo.contains("\tconflicts = myapp-bin"));
    assert!(srcinfo.contains("\tprovides = myapp"));
    assert!(srcinfo.contains("pkgname = myapp"));
}

#[test]
fn test_top_level_aur_sources_config_parsing() {
    use anodizer_core::config::Config;

    let yaml = r#"
project_name: test
aur_sources:
  - name: myapp
    description: "My application"
    license: MIT
    makedepends:
      - rust
      - cargo
    git_url: "ssh://aur@aur.archlinux.org/myapp.git"
  - name: myapp-extra
    description: "Extra package"
    license: MIT
    git_url: "ssh://aur@aur.archlinux.org/myapp-extra.git"
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let aur_sources = config.aur_sources.as_ref().unwrap();
    assert_eq!(aur_sources.len(), 2);
    assert_eq!(aur_sources[0].name.as_deref(), Some("myapp"));
    assert_eq!(
        aur_sources[0].makedepends.as_ref().unwrap(),
        &["rust", "cargo"]
    );
    assert_eq!(aur_sources[1].name.as_deref(), Some("myapp-extra"));
}

#[test]
fn test_aur_source_config_parsing() {
    use anodizer_core::config::Config;

    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur_source:
        name: myapp
        description: "My application"
        license: MIT
        makedepends:
          - rust
          - cargo
        depends:
          - openssl
        git_url: "ssh://aur@aur.archlinux.org/myapp.git"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let aur_src = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .aur_source
        .as_ref()
        .unwrap();
    assert_eq!(aur_src.name.as_deref(), Some("myapp"));
    assert_eq!(aur_src.makedepends.as_ref().unwrap(), &["rust", "cargo"]);
    assert_eq!(aur_src.depends.as_ref().unwrap(), &["openssl"]);
}

#[test]
fn test_aur_source_amd64_variant_field_parses() {
    // amd64_variant lands on AurSourceConfig as a typed Amd64Variant enum
    // (PKGBUILD `prepare:` / `build:` / `package:` template surface uses
    // it as the `Amd64` var; AUR source pkgs don't filter binaries).
    use anodizer_core::config::{Amd64Variant, Config};

    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur_source:
        name: myapp
        description: "My application"
        license: MIT
        amd64_variant: v3
        git_url: "ssh://aur@aur.archlinux.org/myapp.git"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let aur_src = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .aur_source
        .as_ref()
        .unwrap();
    assert_eq!(aur_src.amd64_variant, Some(Amd64Variant::V3));
    assert_eq!(aur_src.amd64_variant.unwrap().as_str(), "v3");
}

#[test]
fn test_aur_source_amd64_variant_typo_rejected() {
    // Typed enum constraint: anything outside v1/v2/v3/v4 must fail at
    // parse time so the bad value never silently lands in the PKGBUILD.
    use anodizer_core::config::Config;

    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur_source:
        name: myapp
        amd64_variant: v9000
"#;
    let result: Result<Config, serde_yaml_ng::Error> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "amd64_variant: v9000 must be rejected by the typed enum"
    );
}

/// Regression:
/// `aur_sources[*].skip_upload: "{{ .IsSnapshot }}"` must
/// template-expand before its bool/auto/empty interpretation. On
/// a snapshot run the rendered value is `"true"` and the publish
/// path must skip the entry without touching git.
#[test]
fn aur_sources_skip_upload_template_expands_to_true_on_snapshot() {
    use anodizer_core::config::{AurSourceConfig, Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.aur_sources = Some(vec![AurSourceConfig {
        // git_url intentionally unset — should_skip_publisher must
        // short-circuit before this becomes a problem.
        description: Some("a thing".to_string()),
        skip_upload: Some(StringOrBool::String("{{ .IsSnapshot }}".to_string())),
        ..Default::default()
    }]);

    let mut ctx = Context::new(
        config,
        ContextOptions {
            snapshot: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("IsSnapshot", "true");

    let log = StageLogger::new("publish", Verbosity::Normal);
    publish_top_level_aur_sources(&mut ctx, &log).expect(
        "skip_upload='{{ .IsSnapshot }}' on snapshot must skip the \
             entry without reaching the git push path (GR cba5b9f)",
    );
}

#[test]
fn generate_source_srcinfo_omits_url_when_homepage_empty() {
    let meta = AurMeta {
        name: "myapp",
        version: "2.3.0",
        pkgrel: 2,
        description: "No homepage tool",
        homepage: "",
        license: &["Apache-2.0".to_string()],
        arches: &["x86_64".to_string(), "aarch64".to_string()],
    };
    let optdepends = vec!["bash-completion: shell completions".to_string()];
    let deps = AurDeps {
        depends: &[],
        makedepends: &[],
        optdepends: &optdepends,
        conflicts: &[],
        provides: &[],
    };
    let srcinfo = generate_source_srcinfo(&meta, &deps, "https://example.com/src-2.3.0.tar.gz");

    // empty homepage -> NO `url =` line.
    assert!(
        !srcinfo.contains("\turl ="),
        "url line must be omitted for empty homepage:\n{srcinfo}"
    );
    // optdepends rendered.
    assert!(srcinfo.contains("\toptdepends = bash-completion: shell completions"));
    // both fixed arches always present.
    assert!(srcinfo.contains("\tarch = x86_64"));
    assert!(srcinfo.contains("\tarch = aarch64"));
    assert!(srcinfo.contains("\tlicense = Apache-2.0"));
    assert!(srcinfo.contains("\tsource = https://example.com/src-2.3.0.tar.gz"));
    assert!(srcinfo.contains("\tsha256sums = SKIP"));
}

#[test]
fn resolve_aur_source_package_name_strip_bin_honors_explicit_name() {
    use anodizer_core::config::AurSourceConfig;
    // Explicit name is taken verbatim, then -bin stripped when requested.
    let cfg = AurSourceConfig {
        name: Some("widget-bin".to_string()),
        ..Default::default()
    };
    assert_eq!(
        resolve_aur_source_package_name(&cfg, "ignored", true),
        "widget"
    );
    // strip disabled -> -bin retained.
    assert_eq!(
        resolve_aur_source_package_name(&cfg, "ignored", false),
        "widget-bin"
    );
}

// -----------------------------------------------------------------------
// render_aur_source_inner — the skip-unaware render the live publish path
// and the offline validator share. Pure (reads ctx, no git): covers source
// URL derivation (GitURL owner extraction for both `://` and `git@host:`
// remotes), the empty-owner warn, the `url_template` override + `Amd64`
// scoping, and the dependency/field defaults landing in the rendered
// PKGBUILD/.SRCINFO.
// -----------------------------------------------------------------------

use anodizer_core::config::{Config, CrateConfig, PublishConfig, StringOrBool};
use anodizer_core::context::ContextOptions;
use anodizer_core::log::Verbosity;

fn quiet_log() -> StageLogger {
    StageLogger::new("publish", Verbosity::Quiet)
}

/// A bare context with the four template vars `render_aur_source_inner`
/// reads (`Version`, `Tag`, `GitURL`, `ProjectName`). The default source
/// URL is `https://github.com/<owner>/<project>/archive/refs/tags/<tag>.tar.gz`,
/// so `owner` comes from `GitURL` and `project` from `ProjectName`.
fn source_ctx(git_url: &str, project: &str, version: &str, tag: &str) -> Context {
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut().set("Version", version);
    ctx.template_vars_mut().set("Tag", tag);
    ctx.template_vars_mut().set("GitURL", git_url);
    ctx.template_vars_mut().set("ProjectName", project);
    ctx
}

/// Default source URL: owner extracted from an `https://` GitURL
/// (`split('/').nth(3)`), project from `ProjectName`, tag from `Tag`. The
/// PKGBUILD `pkgver` carries the `Version` var with hyphens underscored.
#[test]
fn render_inner_default_url_from_https_giturl() {
    let ctx = source_ctx(
        "https://github.com/myorg/mytool.git",
        "mytool",
        "1.2.3-rc1",
        "v1.2.3-rc1",
    );
    let cfg = AurSourceConfig {
        description: Some("A source tool".to_string()),
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    let render = render_aur_source_inner(&ctx, &cfg, "mytool", false, "aur_source", &quiet_log())
        .expect("render ok");
    assert_eq!(render.pkg_name, "mytool");
    // Default source URL points at the github archive tarball.
    assert!(
        render.rendered.pkgbuild.contains(
            "source=(\"https://github.com/myorg/mytool/archive/refs/tags/v1.2.3-rc1.tar.gz\")"
        ),
        "default source URL must derive owner from GitURL + project from ProjectName:\n{}",
        render.rendered.pkgbuild
    );
    // Version hyphen → underscore per AUR pkgver rules.
    assert!(
        render.rendered.pkgbuild.contains("pkgver='1.2.3_rc1'"),
        "{}",
        render.rendered.pkgbuild
    );
    // Default makedepends are rust + cargo.
    assert!(
        render
            .rendered
            .pkgbuild
            .contains("makedepends=('rust' 'cargo')"),
        "{}",
        render.rendered.pkgbuild
    );
    // conflicts/provides default to the bare package name.
    assert!(render.rendered.pkgbuild.contains("conflicts=('mytool')"));
    assert!(render.rendered.pkgbuild.contains("provides=('mytool')"));
}

/// Templated `description` / `homepage` (`{{ .Tag }}`) are
/// template-rendered into the source PKGBUILD `pkgdesc=` / `url=` lines
/// (and the .SRCINFO `pkgdesc =` / `url =` lines) — the literal delimiters
/// must NOT leak. Regression for the raw-emit description/homepage bug.
#[test]
fn render_inner_description_and_homepage_templates_are_rendered() {
    let ctx = source_ctx(
        "https://github.com/myorg/mytool.git",
        "mytool",
        "1.2.3",
        "v1.2.3",
    );
    let cfg = AurSourceConfig {
        description: Some("mytool {{ .Tag }} source build".to_string()),
        homepage: Some("https://example.com/releases/{{ .Tag }}".to_string()),
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    let render = render_aur_source_inner(&ctx, &cfg, "mytool", false, "aur_source", &quiet_log())
        .expect("render ok");
    assert!(
        render
            .rendered
            .pkgbuild
            .contains("pkgdesc=\"mytool v1.2.3 source build\""),
        "templated description must render into PKGBUILD pkgdesc=:\n{}",
        render.rendered.pkgbuild
    );
    assert!(
        render
            .rendered
            .pkgbuild
            .contains("url='https://example.com/releases/v1.2.3'"),
        "templated homepage must render into PKGBUILD url=:\n{}",
        render.rendered.pkgbuild
    );
    assert!(
        !render.rendered.pkgbuild.contains("{{"),
        "PKGBUILD must carry no unrendered `{{{{`:\n{}",
        render.rendered.pkgbuild
    );
    assert!(
        render
            .rendered
            .srcinfo
            .contains("pkgdesc = mytool v1.2.3 source build"),
        ".SRCINFO pkgdesc must carry the resolved value:\n{}",
        render.rendered.srcinfo
    );
    assert!(
        render
            .rendered
            .srcinfo
            .contains("url = https://example.com/releases/v1.2.3"),
        ".SRCINFO url must carry the resolved value:\n{}",
        render.rendered.srcinfo
    );
    assert!(
        !render.rendered.srcinfo.contains("{{"),
        ".SRCINFO must carry no unrendered `{{{{`:\n{}",
        render.rendered.srcinfo
    );
}

/// Default source URL owner extraction for an SCP-style `git@host:owner/repo`
/// remote (the `contains(':')` branch, `split(':').nth(1).split('/').next()`).
#[test]
fn render_inner_default_url_from_scp_giturl() {
    let ctx = source_ctx(
        "git@github.com:acme/widget.git",
        "widget",
        "2.0.0",
        "v2.0.0",
    );
    let cfg = AurSourceConfig::default();
    let render = render_aur_source_inner(&ctx, &cfg, "widget", false, "aur_source", &quiet_log())
        .expect("render ok");
    assert!(
        render.rendered.pkgbuild.contains(
            "source=(\"https://github.com/acme/widget/archive/refs/tags/v2.0.0.tar.gz\")"
        ),
        "SCP-style GitURL owner must extract to 'acme':\n{}",
        render.rendered.pkgbuild
    );
}

/// An unparseable GitURL (no scheme, no `:`) yields an empty owner; the
/// renderer warns and still produces a (malformed-owner) source URL rather
/// than panicking.
#[test]
fn render_inner_empty_owner_warns_and_continues() {
    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = source_ctx("not-a-url", "thing", "1.0.0", "v1.0.0");
    ctx.with_log_capture(capture.clone());
    let log = ctx.logger("publish");
    let cfg = AurSourceConfig::default();
    let render = render_aur_source_inner(&ctx, &cfg, "thing", false, "aur_source", &log)
        .expect("render ok despite unextractable owner");
    // Empty owner → URL has an empty owner segment.
    assert!(
        render
            .rendered
            .pkgbuild
            .contains("source=(\"https://github.com//thing/archive/refs/tags/v1.0.0.tar.gz\")"),
        "{}",
        render.rendered.pkgbuild
    );
    assert!(
        capture
            .warn_messages()
            .iter()
            .any(|m| m.contains("could not extract owner")),
        "an unextractable GitURL must warn the operator; got: {:?}",
        capture.warn_messages()
    );
}

/// `url_template` overrides the default github-archive URL and sees the
/// `Amd64` micro-architecture var (default `v1`) plus the standard vars.
#[test]
fn render_inner_url_template_overrides_with_amd64_scope() {
    let ctx = source_ctx("https://github.com/o/p.git", "p", "3.1.0", "v3.1.0");
    let cfg = AurSourceConfig {
        url_template: Some("https://dl.example/{{ .Version }}/{{ .Amd64 }}/src.tar.gz".to_string()),
        ..Default::default()
    };
    let render = render_aur_source_inner(&ctx, &cfg, "p", false, "aur_source", &quiet_log())
        .expect("render ok");
    assert!(
        render
            .rendered
            .pkgbuild
            .contains("source=(\"https://dl.example/3.1.0/v1/src.tar.gz\")"),
        "url_template must render with default Amd64=v1:\n{}",
        render.rendered.pkgbuild
    );
    // The scoped vars threaded out carry the same Amd64 the render saw.
    assert_eq!(
        render.scoped_vars.get("Amd64").map(|s| s.as_str()),
        Some("v1")
    );
}

/// A configured `amd64_variant` surfaces as the `Amd64` template var the
/// `url_template` (and hook bodies) branch on.
#[test]
fn render_inner_amd64_variant_threads_into_template() {
    use anodizer_core::config::Amd64Variant;
    let ctx = source_ctx("https://github.com/o/p.git", "p", "1.0.0", "v1.0.0");
    let cfg = AurSourceConfig {
        amd64_variant: Some(Amd64Variant::V3),
        url_template: Some("https://dl/{{ .Amd64 }}.tar.gz".to_string()),
        ..Default::default()
    };
    let render = render_aur_source_inner(&ctx, &cfg, "p", false, "aur_source", &quiet_log())
        .expect("render ok");
    assert!(
        render
            .rendered
            .pkgbuild
            .contains("source=(\"https://dl/v3.tar.gz\")"),
        "{}",
        render.rendered.pkgbuild
    );
    assert_eq!(
        render.scoped_vars.get("Amd64").map(|s| s.as_str()),
        Some("v3")
    );
}

/// `install:` set → the render reports `<pkg>.install` and the PKGBUILD
/// emits the `install=` line; unset → no install filename reference leaks
/// into the body.
#[test]
fn render_inner_install_filename_tracks_config() {
    let ctx = source_ctx("https://github.com/o/p.git", "p", "1.0.0", "v1.0.0");
    let cfg = AurSourceConfig {
        install: Some("post_install() { :; }".to_string()),
        ..Default::default()
    };
    let render = render_aur_source_inner(&ctx, &cfg, "p", false, "aur_source", &quiet_log())
        .expect("render ok");
    assert_eq!(render.install_filename, "p.install");
    assert!(
        render.rendered.pkgbuild.contains("install=p.install"),
        "{}",
        render.rendered.pkgbuild
    );

    let cfg_none = AurSourceConfig::default();
    let render_none =
        render_aur_source_inner(&ctx, &cfg_none, "p", false, "aur_source", &quiet_log())
            .expect("render ok");
    assert!(
        !render_none.rendered.pkgbuild.contains("install="),
        "no install= line when install unset:\n{}",
        render_none.rendered.pkgbuild
    );
}

/// Top-level entries strip a trailing `-bin` from the default name; the
/// `Version` default `0.0.0` applies when the var is absent.
#[test]
fn render_inner_strips_bin_and_defaults_version() {
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    // No Version var → defaults to 0.0.0; GitURL/ProjectName empty.
    ctx.template_vars_mut().set("Tag", "v9");
    let cfg = AurSourceConfig::default();
    let render =
        render_aur_source_inner(&ctx, &cfg, "foo-bin", true, "aur_sources[0]", &quiet_log())
            .expect("render ok");
    assert_eq!(
        render.pkg_name, "foo",
        "-bin must be stripped for top-level"
    );
    assert!(
        render.rendered.pkgbuild.contains("pkgver='0.0.0'"),
        "missing Version var must default to 0.0.0:\n{}",
        render.rendered.pkgbuild
    );
}

/// Workspace per-crate mode: an `aur_sources[]`/`aur_source` entry for crate
/// `bravo` that omits description/homepage/license/maintainers must resolve
/// each through `bravo`'s OWN `Cargo.toml` metadata — never crate `alfa`'s
/// (no cross-crate leakage), never the crate name as description, never an
/// empty url, and never a hardcoded `MIT` license. Mirrors the `-bin` AUR
/// publisher's `meta_*_for(<crate>)` resolution.
#[test]
fn render_inner_per_crate_metadata_no_cross_crate_leakage() {
    use anodizer_core::config::MetadataConfig;

    let mut ctx = source_ctx("https://github.com/o/ws.git", "ws", "1.0.0", "v1.0.0");
    // Two crates' derived Cargo.toml metadata, as populate_derived_metadata
    // would key them. `bravo` carries a non-MIT license and a real homepage.
    ctx.config.derived_metadata.insert(
        "alfa".to_string(),
        MetadataConfig {
            description: Some("Alfa the first tool".to_string()),
            homepage: Some("https://alfa.example".to_string()),
            license: Some("Apache-2.0".to_string()),
            maintainers: Some(vec!["Alfa Author <alfa@example.com>".to_string()]),
            ..Default::default()
        },
    );
    ctx.config.derived_metadata.insert(
        "bravo".to_string(),
        MetadataConfig {
            description: Some("Bravo the second tool".to_string()),
            homepage: Some("https://bravo.example".to_string()),
            license: Some("GPL-3.0-or-later".to_string()),
            maintainers: Some(vec!["Bravo Author <bravo@example.com>".to_string()]),
            ..Default::default()
        },
    );

    // The `bravo` entry omits every metadata field — they must come from
    // bravo's own Cargo.toml, resolved by default_name = "bravo".
    let cfg = AurSourceConfig::default();
    let render = render_aur_source_inner(&ctx, &cfg, "bravo", false, "aur_source", &quiet_log())
        .expect("render ok");
    let pkgbuild = &render.rendered.pkgbuild;

    assert!(
        pkgbuild.contains("pkgdesc=\"Bravo the second tool\""),
        "description must be bravo's real Cargo.toml description, not the \
             crate name or alfa's:\n{}",
        pkgbuild
    );
    assert!(
        pkgbuild.contains("url='https://bravo.example'"),
        "homepage/url must be bravo's real Cargo.toml homepage, not empty or \
             alfa's:\n{}",
        pkgbuild
    );
    assert!(
        pkgbuild.contains("license=('GPL-3.0-or-later')"),
        "license must be bravo's real Cargo.toml license, not a hardcoded MIT \
             or alfa's:\n{}",
        pkgbuild
    );
    assert!(
        pkgbuild.contains("# Maintainer: Bravo Author <bravo@example.com>"),
        "maintainer must be bravo's real Cargo.toml author, not empty or \
             alfa's:\n{}",
        pkgbuild
    );

    // Prove no alfa leakage anywhere in the rendered artifact.
    assert!(
        !pkgbuild.contains("alfa") && !pkgbuild.contains("Alfa") && !pkgbuild.contains("Apache"),
        "crate alfa's metadata must not leak into bravo's PKGBUILD:\n{}",
        pkgbuild
    );
    assert!(
        !pkgbuild.contains("license=('MIT')"),
        "license must never fall back to a hardcoded MIT:\n{}",
        pkgbuild
    );

    // Explicit config still wins over the resolver: an entry that DOES set a
    // field uses the literal value, not the crate metadata.
    let cfg_explicit = AurSourceConfig {
        license: Some("BSD-3-Clause".to_string()),
        ..Default::default()
    };
    let render_explicit = render_aur_source_inner(
        &ctx,
        &cfg_explicit,
        "bravo",
        false,
        "aur_source",
        &quiet_log(),
    )
    .expect("render ok");
    assert!(
        render_explicit
            .rendered
            .pkgbuild
            .contains("license=('BSD-3-Clause')"),
        "explicit license must override the crate-metadata fallback:\n{}",
        render_explicit.rendered.pkgbuild
    );
}

// -----------------------------------------------------------------------
// render_aur_source_pkgbuild_and_srcinfo_for_crate / render_top_level_aur_source
// — the skip-aware entry points the offline validator drives. Pure (no git).
// -----------------------------------------------------------------------

fn crate_with_aur_source(name: &str, cfg: AurSourceConfig) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            aur_source: Some(cfg),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// No `aur_source` block on the crate → `Ok(None)` (the validator treats it
/// as nothing to validate).
#[test]
fn render_per_crate_none_when_no_block() {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "demo".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig::default()),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut()
        .set("GitURL", "https://github.com/o/demo.git");
    ctx.template_vars_mut().set("ProjectName", "demo");
    let out = render_aur_source_pkgbuild_and_srcinfo_for_crate(&ctx, "demo", &quiet_log())
        .expect("render ok");
    assert!(out.is_none(), "no aur_source block → None");
}

/// A configured crate renders the PKGBUILD/.SRCINFO byte-content the live
/// publish would push.
#[test]
fn render_per_crate_emits_pkgbuild() {
    let mut config = Config::default();
    config.crates = vec![crate_with_aur_source(
        "demo",
        AurSourceConfig {
            description: Some("demo tool".to_string()),
            ..Default::default()
        },
    )];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut()
        .set("GitURL", "https://github.com/o/demo.git");
    ctx.template_vars_mut().set("ProjectName", "demo");
    let rendered = render_aur_source_pkgbuild_and_srcinfo_for_crate(&ctx, "demo", &quiet_log())
        .expect("render ok")
        .expect("not skipped");
    assert_eq!(rendered.package_name, "demo");
    assert!(
        rendered.pkgbuild.contains("pkgname='demo'"),
        "{}",
        rendered.pkgbuild
    );
    assert!(
        rendered.srcinfo.contains("pkgbase = demo"),
        "{}",
        rendered.srcinfo
    );
}

/// A truthy `skip` on the per-crate block short-circuits to `Ok(None)`.
#[test]
fn render_per_crate_skip_true_returns_none() {
    let mut config = Config::default();
    config.crates = vec![crate_with_aur_source(
        "demo",
        AurSourceConfig {
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        },
    )];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut()
        .set("GitURL", "https://github.com/o/demo.git");
    ctx.template_vars_mut().set("ProjectName", "demo");
    let out = render_aur_source_pkgbuild_and_srcinfo_for_crate(&ctx, "demo", &quiet_log())
        .expect("render ok");
    assert!(out.is_none(), "skip=true → None");
}

/// A workspace-only crate (pure-workspace config) renders: the emission
/// validator drives this entry point per crate, so an `Ok(None)` here
/// would silently exclude the workspace crate's PKGBUILD from
/// validation while the live publish still pushes it.
#[test]
fn render_per_crate_resolves_workspace_only_crate() {
    let mut config = Config::default();
    config.workspaces = Some(vec![anodizer_core::config::WorkspaceConfig {
        name: "ws".to_string(),
        crates: vec![crate_with_aur_source(
            "ws-only",
            AurSourceConfig {
                description: Some("ws tool".to_string()),
                ..Default::default()
            },
        )],
        ..Default::default()
    }]);
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut()
        .set("GitURL", "https://github.com/o/ws-only.git");
    ctx.template_vars_mut().set("ProjectName", "ws-only");
    let rendered = render_aur_source_pkgbuild_and_srcinfo_for_crate(&ctx, "ws-only", &quiet_log())
        .expect("render ok")
        .expect("workspace-only crate must resolve, not silently skip");
    assert_eq!(rendered.package_name, "ws-only");
}

/// Top-level `aur_sources` array: empty/unset → empty Vec; populated →
/// one rendered artifact per non-skipped entry; a truthy `skip` drops the
/// entry.
#[test]
fn render_top_level_handles_empty_populated_and_skip() {
    // Unset → empty Vec.
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut()
        .set("GitURL", "https://github.com/o/p.git");
    ctx.template_vars_mut().set("ProjectName", "p");
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set("Version", "1.0.0");
    assert!(
        render_top_level_aur_source(&ctx, &quiet_log())
            .expect("render ok")
            .is_empty(),
        "unset aur_sources → empty Vec"
    );

    // Two entries, the second skipped → only the first renders.
    let mut config = Config::default();
    config.project_name = "p".to_string();
    config.aur_sources = Some(vec![
        AurSourceConfig {
            name: Some("first".to_string()),
            ..Default::default()
        },
        AurSourceConfig {
            name: Some("second".to_string()),
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        },
    ]);
    let mut ctx2 = Context::new(config, ContextOptions::default());
    ctx2.template_vars_mut()
        .set("GitURL", "https://github.com/o/p.git");
    ctx2.template_vars_mut().set("ProjectName", "p");
    ctx2.template_vars_mut().set("Tag", "v1.0.0");
    ctx2.template_vars_mut().set("Version", "1.0.0");
    let out = render_top_level_aur_source(&ctx2, &quiet_log()).expect("render ok");
    assert_eq!(out.len(), 1, "skipped entry must be dropped");
    assert_eq!(out[0].package_name, "first");
}

// -----------------------------------------------------------------------
// Dry-run publish — exercises the `is_dry_run` early-exit in
// `publish_aur_source_entry` without touching git.
// -----------------------------------------------------------------------

/// In dry-run, `publish_aur_source_entry` (via `publish_to_aur_source`)
/// returns `Ok(false)` (nothing pushed) before any clone/write.
#[test]
fn publish_to_aur_source_dry_run_returns_false() {
    let mut config = Config::default();
    config.crates = vec![crate_with_aur_source(
        "demo",
        AurSourceConfig {
            description: Some("demo".to_string()),
            ..Default::default()
        },
    )];
    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut()
        .set("GitURL", "https://github.com/o/demo.git");
    ctx.template_vars_mut().set("ProjectName", "demo");
    let pushed = publish_to_aur_source(&mut ctx, "demo", &quiet_log()).expect("dry-run ok");
    assert!(!pushed, "dry-run must not push");
}

/// `publish_to_aur_source` errors when the named crate carries no
/// `aur_source` block at all (the `ok_or_else` on the missing config).
#[test]
fn publish_to_aur_source_missing_block_errors() {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "demo".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig::default()),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    let err = publish_to_aur_source(&mut ctx, "demo", &quiet_log())
        .expect_err("missing aur_source must error");
    assert!(
        format!("{err:#}").contains("no aur_source config"),
        "{err:#}"
    );
}

// -----------------------------------------------------------------------
// Live git-over-ssh source publish — clone a local bare repo, write
// PKGBUILD/.SRCINFO/.install, commit, push to `master`. `#[cfg(unix)]`-gated:
// spawns git, sets commit-identity env, asserts pushed bytes on the bare
// ref. Precedent: aur.rs `make_bare_aur_repo`.
// -----------------------------------------------------------------------

#[cfg(unix)]
fn git_ok(dir: &std::path::Path, args: &[&str]) {
    anodizer_core::test_helpers::git_test_ok(dir, args)
}

#[cfg(unix)]
fn git_stdout(dir: &std::path::Path, args: &[&str]) -> String {
    anodizer_core::test_helpers::git_test_stdout(dir, args)
}

/// A bare AUR repo seeded with one commit on `master`. Returns a usable
/// local clone URL plus the holder tempdir.
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

/// Build a per-crate source-publish context pointing the clone at a local
/// bare repo, with the four template vars the render reads populated.
#[cfg(unix)]
fn live_source_ctx(bare_url: &str, cfg_mut: impl FnOnce(&mut AurSourceConfig)) -> Context {
    let mut cfg = AurSourceConfig {
        git_url: Some(bare_url.to_string()),
        description: Some("A source tool".to_string()),
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    cfg_mut(&mut cfg);
    let mut config = Config::default();
    config.dist = std::env::temp_dir().join(format!(
        "anodize-aursrc-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    config.crates = vec![crate_with_aur_source("mytool", cfg)];
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.2.3");
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    ctx.template_vars_mut()
        .set("GitURL", "https://github.com/myorg/mytool.git");
    ctx.template_vars_mut().set("ProjectName", "mytool");
    ctx
}

/// End-to-end per-crate source publish: clone, write, commit, push. Assert
/// the pushed PKGBUILD pkgname + .SRCINFO pkgbase and the `true` outcome.
#[cfg(unix)]
#[test]
fn publish_to_aur_source_pushes_to_master() {
    let (bare_url, bare) = make_bare_aur_repo();
    let mut ctx = live_source_ctx(&bare_url, |_| {});
    let pushed = publish_to_aur_source(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
    assert!(pushed, "a fresh source PKGBUILD must report a push");

    let pkgbuild = aur_show(std::path::Path::new(&bare_url), "PKGBUILD");
    assert!(pkgbuild.contains("pkgname='mytool'"), "{pkgbuild}");
    assert!(
        pkgbuild.contains(
            "source=(\"https://github.com/myorg/mytool/archive/refs/tags/v1.2.3.tar.gz\")"
        ),
        "{pkgbuild}"
    );
    let srcinfo = aur_show(std::path::Path::new(&bare_url), ".SRCINFO");
    assert!(srcinfo.contains("pkgbase = mytool"), "{srcinfo}");
    std::fs::remove_dir_all(&ctx.config.dist).ok();
    drop(bare);
}

/// A second publish against an unchanged repo reports `NoChanges` → `false`.
#[cfg(unix)]
#[test]
fn publish_to_aur_source_second_run_no_changes_returns_false() {
    let (bare_url, bare) = make_bare_aur_repo();
    let mut ctx = live_source_ctx(&bare_url, |_| {});
    assert!(
        publish_to_aur_source(&mut ctx, "mytool", &quiet_log()).expect("first publish ok"),
        "first publish must push"
    );
    assert!(
        !publish_to_aur_source(&mut ctx, "mytool", &quiet_log()).expect("second publish ok"),
        "an unchanged repo must report no push"
    );
    std::fs::remove_dir_all(&ctx.config.dist).ok();
    drop(bare);
}

/// `install:` set → the `.install` file lands on `master` and the PKGBUILD
/// references it. Also drives the `git_ssh_command` clone branch (a no-op
/// `ssh` command; the local-path clone ignores `GIT_SSH_COMMAND`).
#[cfg(unix)]
#[test]
fn publish_to_aur_source_writes_install_and_uses_ssh_branch() {
    let (bare_url, bare) = make_bare_aur_repo();
    let mut ctx = live_source_ctx(&bare_url, |c| {
        c.install = Some("post_install() { echo hi; }".to_string());
        // Non-empty git_ssh_command routes through `clone_repo_ssh`; for a
        // local-path clone git ignores GIT_SSH_COMMAND so the clone still
        // succeeds, exercising the SSH branch's config-write path.
        c.git_ssh_command = Some("ssh -o StrictHostKeyChecking=no".to_string());
    });
    assert!(publish_to_aur_source(&mut ctx, "mytool", &quiet_log()).expect("publish ok"));
    let pkgbuild = aur_show(std::path::Path::new(&bare_url), "PKGBUILD");
    assert!(pkgbuild.contains("install=mytool.install"), "{pkgbuild}");
    let install = aur_show(std::path::Path::new(&bare_url), "mytool.install");
    assert_eq!(install, "post_install() { echo hi; }");
    std::fs::remove_dir_all(&ctx.config.dist).ok();
    drop(bare);
}

/// `directory:` nests the committed files under a subdirectory rendered
/// from the template (with the `Amd64` var scoped in).
#[cfg(unix)]
#[test]
fn publish_to_aur_source_directory_nests_output() {
    let (bare_url, bare) = make_bare_aur_repo();
    let mut ctx = live_source_ctx(&bare_url, |c| {
        c.directory = Some("pkgs/{{ .Amd64 }}".to_string());
    });
    assert!(publish_to_aur_source(&mut ctx, "mytool", &quiet_log()).expect("publish ok"));
    // Amd64 defaults to v1, so the files land under pkgs/v1/.
    let pkgbuild = aur_show(std::path::Path::new(&bare_url), "pkgs/v1/PKGBUILD");
    assert!(pkgbuild.contains("pkgname='mytool'"), "{pkgbuild}");
    std::fs::remove_dir_all(&ctx.config.dist).ok();
    drop(bare);
}

/// Cloning a non-repo path fails; the error names the `aur_source` label.
#[cfg(unix)]
#[test]
fn publish_to_aur_source_clone_failure_errors() {
    let bogus = tempfile::tempdir().expect("bogus dir");
    let bogus_url = bogus.path().to_string_lossy().into_owned();
    let mut ctx = live_source_ctx(&bogus_url, |_| {});
    let err = publish_to_aur_source(&mut ctx, "mytool", &quiet_log())
        .expect_err("cloning a non-repo path must fail");
    assert!(
        format!("{err:#}").contains("aur_source"),
        "error must name the label: {err:#}"
    );
    std::fs::remove_dir_all(&ctx.config.dist).ok();
    drop(bogus);
}

/// Full `Publisher::run` over a per-crate source block pushes the package,
/// records exactly one target carrying the pushed git_url + tag, and
/// `rollback` warns (irreversible force-push) without error.
#[cfg(unix)]
#[test]
fn aur_source_publisher_run_pushes_and_records_target() {
    use anodizer_core::Publisher;
    let (bare_url, bare) = make_bare_aur_repo();
    // Point project_root at a hermetic `v0.1.0`-tagged repo so the per-crate
    // scope resolves the crate's tag deterministically (its `tag_template`
    // is `v{{ .Version }}`), rather than depending on the process cwd's tags.
    let scope_repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = live_source_ctx(&bare_url, |_| {});
    ctx.options.project_root = Some(scope_repo.path().to_path_buf());
    ctx.options.selected_crates = vec!["mytool".to_string()];
    let p = AurSourcePublisher::new();

    let evidence = p.run(&mut ctx).expect("run ok");
    let targets = decode_aur_source_targets(&evidence.extra);
    assert_eq!(targets.len(), 1, "one push → one recorded target");
    assert_eq!(targets[0].package, "mytool");
    assert_eq!(targets[0].git_url, bare_url);
    assert_eq!(
        evidence.primary_ref.as_deref(),
        Some("https://aur.archlinux.org/packages/mytool"),
        "primary_ref must point at the AUR package page"
    );

    // The package landed on master.
    let pkgbuild = aur_show(std::path::Path::new(&bare_url), "PKGBUILD");
    assert!(pkgbuild.contains("pkgname='mytool'"), "{pkgbuild}");

    // Rollback is warn-only (force-push is irreversible); must not error.
    p.rollback(&mut ctx, &evidence).expect("rollback ok");
    std::fs::remove_dir_all(&ctx.config.dist).ok();
    drop(bare);
}

/// `Publisher::run` with a top-level `aur_sources` entry pushes it and
/// records the `aur_sources[0]` target.
#[cfg(unix)]
#[test]
fn aur_source_publisher_run_pushes_top_level_entry() {
    use anodizer_core::Publisher;
    let (bare_url, bare) = make_bare_aur_repo();
    let mut config = Config::default();
    config.project_name = "widget".to_string();
    config.dist = std::env::temp_dir().join(format!(
        "anodize-aursrc-top-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    config.aur_sources = Some(vec![AurSourceConfig {
        git_url: Some(bare_url.clone()),
        description: Some("widget tool".to_string()),
        ..Default::default()
    }]);
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "2.0.0");
    ctx.template_vars_mut().set("Tag", "v2.0.0");
    ctx.template_vars_mut()
        .set("GitURL", "https://github.com/myorg/widget.git");
    ctx.template_vars_mut().set("ProjectName", "widget");
    let p = AurSourcePublisher::new();

    let evidence = p.run(&mut ctx).expect("run ok");
    let targets = decode_aur_source_targets(&evidence.extra);
    assert_eq!(targets.len(), 1, "one top-level entry → one target");
    assert_eq!(targets[0].target, "aur_sources[0]");
    assert_eq!(targets[0].package, "widget");

    let srcinfo = aur_show(std::path::Path::new(&bare_url), ".SRCINFO");
    assert!(srcinfo.contains("pkgbase = widget"), "{srcinfo}");
    std::fs::remove_dir_all(&ctx.config.dist).ok();
    drop(bare);
}

/// `aur_source.private_key` templates are rendered against the
/// context env vars before the key bytes reach the SSH key file. A
/// literal `{{ .Env.X }}` written to the file would fail `ssh` at
/// parse time with "error in libcrypto". Mirrors the analogous
/// `aur_clone_repo_renders_templated_private_key_before_write` test
/// in `aur.rs`.
#[cfg(unix)]
#[test]
fn aur_source_renders_templated_private_key_before_write() {
    let (bare_url, bare) = make_bare_aur_repo();
    let key_value =
        "-----BEGIN OPENSSH PRIVATE KEY-----\nZZZZ\n-----END OPENSSH PRIVATE KEY-----\n";

    // Build a context with the templated private_key and the env var
    // that the template references. `render_or_warn_with_vars` is the
    // same function `publish_to_aur_source` calls on `private_key`
    // before passing the rendered bytes to `clone_repo_ssh`.
    let mut ctx = live_source_ctx(&bare_url, |c| {
        c.private_key = Some("{{ .Env.AUR_SOURCE_TEST_KEY }}".to_string());
    });
    ctx.template_vars_mut()
        .set_env("AUR_SOURCE_TEST_KEY", key_value);

    // Render via the same path the production code takes.
    let log = quiet_log();
    let rendered = util::render_or_warn(
        &ctx,
        &log,
        "aur_source.private_key",
        "{{ .Env.AUR_SOURCE_TEST_KEY }}",
    )
    .expect("render must succeed when env var is set");
    assert_eq!(
        rendered, key_value,
        "rendered private_key must equal the env var value, not the literal template"
    );
    assert!(
        !rendered.contains("{{"),
        "the literal template must never appear in the rendered key"
    );

    // Also verify the full publish path: clone the bare repo with the
    // rendered key so the key file is actually written to disk. Since
    // the clone is local-path, `GIT_SSH_COMMAND` is ignored and the
    // clone succeeds regardless of key validity, letting us confirm
    // the render → write path end-to-end without a real SSH server.
    let parent = tempfile::tempdir().expect("parent");
    let dest = parent.path().join("clone");
    util::clone_repo_ssh(&bare_url, Some(&rendered), None, &dest, "aur_source", &log)
        .expect("clone with rendered key must succeed");
    let key_path = dest.join(".git").join("anodizer_ssh_key");
    let written = std::fs::read_to_string(&key_path).expect("persisted key must be written");
    assert_eq!(
        written.trim_end_matches('\n'),
        key_value.trim_end_matches('\n'),
        "key file must contain the rendered env var value, never the literal template"
    );
    assert!(
        !written.contains("{{"),
        "literal template must never reach the SSH key file"
    );

    std::fs::remove_dir_all(&ctx.config.dist).ok();
    drop(bare);
    drop(parent);
}

/// `Publisher::run` in dry-run records no targets (no push happened).
#[test]
fn aur_source_publisher_run_dry_run_records_no_targets() {
    use anodizer_core::Publisher;
    let mut config = Config::default();
    config.crates = vec![crate_with_aur_source(
        "demo",
        AurSourceConfig {
            git_url: Some("ssh://aur@aur.archlinux.org/demo.git".to_string()),
            description: Some("demo".to_string()),
            ..Default::default()
        },
    )];
    // Point project_root at a hermetic `v0.1.0`-tagged repo so the per-crate
    // scope resolves "demo"'s tag (`v{{ .Version }}`) deterministically
    // rather than from the process cwd's tags, which a checkout with no
    // fetched tags (CI) leaves empty — starving the resolution.
    let scope_repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            project_root: Some(scope_repo.path().to_path_buf()),
            ..Default::default()
        },
    );
    ctx.options.selected_crates = vec!["demo".to_string()];
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut()
        .set("GitURL", "https://github.com/o/demo.git");
    ctx.template_vars_mut().set("ProjectName", "demo");
    let p = AurSourcePublisher::new();
    let evidence = p.run(&mut ctx).expect("dry-run run ok");
    let targets = decode_aur_source_targets(&evidence.extra);
    assert!(
        targets.is_empty(),
        "dry-run must not record force-push targets: {targets:?}"
    );
}
