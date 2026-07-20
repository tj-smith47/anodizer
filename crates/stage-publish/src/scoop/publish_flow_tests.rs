use super::*;
use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{
    Config, CrateConfig, GitRepoConfig, PublishConfig, ReleaseConfig, RepositoryConfig,
    ScmRepoConfig, ScoopConfig, StringOrBool,
};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::log::{StageLogger, Verbosity};
use std::collections::HashMap;

fn quiet() -> StageLogger {
    StageLogger::new("publish", Verbosity::Quiet)
}

fn build_ctx(crates: Vec<CrateConfig>, version: &str) -> Context {
    let config = Config {
        crates,
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", version);
    ctx.template_vars_mut().set("RawVersion", version);
    ctx.template_vars_mut().set("Tag", &format!("v{version}"));
    ctx.template_vars_mut().set("ProjectName", "widget");
    ctx
}

/// A scoop crate whose bucket clones from a local bare repo (`git.url`).
/// `release.github = acme/widget` provides the homepage-slug fallback.
fn scoop_crate_for_bucket(crate_name: &str, bucket_url: &str) -> CrateConfig {
    CrateConfig {
        name: crate_name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        release: Some(ReleaseConfig {
            github: Some(ScmRepoConfig {
                owner: "acme".to_string(),
                name: "widget".to_string(),
                token: None,
            }),
            ..Default::default()
        }),
        publish: Some(PublishConfig {
            scoop: Some(ScoopConfig {
                repository: Some(RepositoryConfig {
                    owner: Some("acme".to_string()),
                    name: Some("scoop-bucket".to_string()),
                    branch: Some("main".to_string()),
                    token: Some("ghp_test".to_string()),
                    git: Some(GitRepoConfig {
                        url: Some(bucket_url.to_string()),
                        ssh_command: None,
                        private_key: None,
                    }),
                    ..Default::default()
                }),
                description: Some("Manage widgets from Windows".to_string()),
                license: Some("MIT".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Register one Windows archive artifact carrying the `url` / `sha256` /
/// `binary` / `format` metadata the manifest's `architecture` block reads.
fn add_windows_archive(
    ctx: &mut Context,
    crate_name: &str,
    target: &str,
    arch: &str,
    binary: &str,
    sha: &str,
) {
    let mut meta = HashMap::new();
    meta.insert(
        "url".to_string(),
        format!(
            "https://github.com/acme/widget/releases/download/v1.0.0/{binary}-windows-{arch}.zip"
        ),
    );
    meta.insert("sha256".to_string(), sha.to_string());
    meta.insert("format".to_string(), "zip".to_string());
    meta.insert("binary".to_string(), binary.to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("/dist/{binary}-windows-{arch}.zip")),
        name: format!("{binary}-windows-{arch}.zip"),
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata: meta,
        size: None,
    });
}

/// scoop installs by unzipping an archive — it cannot run an installer. A
/// `use: msi`/`nsis`/`wix`/`exe` config must therefore be rejected with a
/// clear, actionable error at selection rather than emit a manifest whose
/// `architecture.<arch>.url` points at an installer scoop cannot execute.
#[test]
fn scoop_rejects_msi_use_artifact() {
    let mut crate_cfg = scoop_crate_for_bucket("widget", "file:///tmp/unused");
    crate_cfg
        .publish
        .as_mut()
        .unwrap()
        .scoop
        .as_mut()
        .unwrap()
        .use_artifact = Some("msi".to_string());
    let mut ctx = build_ctx(vec![crate_cfg], "1.0.0");
    add_windows_archive(
        &mut ctx,
        "widget",
        "x86_64-pc-windows-msvc",
        "x64",
        "widget",
        &"a".repeat(64),
    );

    let err = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect_err("scoop must reject use: msi");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("scoop") && msg.contains("msi") && msg.contains("archive"),
        "error must name scoop + the bad use value + that scoop is archive-only; got: {msg}"
    );
}

/// The `nsis` arm of the archive-only gate, ensuring every installer `use:`
/// value (not just `msi`) is rejected.
#[test]
fn scoop_rejects_nsis_use_artifact() {
    let mut crate_cfg = scoop_crate_for_bucket("widget", "file:///tmp/unused");
    crate_cfg
        .publish
        .as_mut()
        .unwrap()
        .scoop
        .as_mut()
        .unwrap()
        .use_artifact = Some("nsis".to_string());
    let mut ctx = build_ctx(vec![crate_cfg], "1.0.0");
    add_windows_archive(
        &mut ctx,
        "widget",
        "x86_64-pc-windows-msvc",
        "x64",
        "widget",
        &"a".repeat(64),
    );

    let err = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect_err("scoop must reject use: nsis");
    assert!(
        format!("{err:#}").contains("nsis"),
        "error must name the bad use value; got: {err:#}"
    );
}

/// The default (archive) config and an explicit `use: archive` both stay on
/// the working zip path — the gate must not regress valid configs.
#[test]
fn scoop_accepts_archive_use_artifact() {
    let mut crate_cfg = scoop_crate_for_bucket("widget", "file:///tmp/unused");
    crate_cfg
        .publish
        .as_mut()
        .unwrap()
        .scoop
        .as_mut()
        .unwrap()
        .use_artifact = Some("archive".to_string());
    let mut ctx = build_ctx(vec![crate_cfg], "1.0.0");
    add_windows_archive(
        &mut ctx,
        "widget",
        "x86_64-pc-windows-msvc",
        "x64",
        "widget",
        &"a".repeat(64),
    );

    let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("use: archive must render")
        .expect("not skipped");
    assert!(manifest.contains("\"64bit\""), "got:\n{manifest}");
}

/// Register a Windows `.tar.gz` archive on `target`/`arch` — the non-`.zip`
/// format scoop also accepts. Pairs with [`add_windows_archive`] to build a
/// package whose windows assets are not uniformly `.zip`.
fn add_windows_targz_archive(
    ctx: &mut Context,
    crate_name: &str,
    target: &str,
    arch: &str,
    binary: &str,
    sha: &str,
) {
    let mut meta = HashMap::new();
    meta.insert(
        "url".to_string(),
        format!(
            "https://github.com/acme/widget/releases/download/v1.0.0/{binary}-windows-{arch}.tar.gz"
        ),
    );
    meta.insert("sha256".to_string(), sha.to_string());
    meta.insert("format".to_string(), "tar.gz".to_string());
    meta.insert("binary".to_string(), binary.to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("/dist/{binary}-windows-{arch}.tar.gz")),
        name: format!("{binary}-windows-{arch}.tar.gz"),
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata: meta,
        size: None,
    });
}

/// Split mode, custom `name_template` that embeds `{{ ArtifactExt }}` in the
/// sidecar SUFFIX, with non-uniform windows assets (one `.zip`, one
/// `.tar.gz`). Scoop's `$url.<suffix>` is a single static string, so a
/// per-asset-varying extension would 404 for the non-matching asset — the
/// render must HARD-FAIL naming the crate + template rather than bake a
/// guessed `zip.sha256` suffix.
#[test]
fn render_scoop_autoupdate_sidecar_artifactext_in_suffix_non_uniform_errors() {
    use anodizer_core::config::ChecksumConfig;
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    c.checksum = Some(ChecksumConfig {
        split: Some(true),
        name_template: Some("{{ ArtifactName }}.{{ ArtifactExt }}.sha256".to_string()),
        ..Default::default()
    });
    let mut ctx = build_ctx(vec![c], "1.0.0");
    // Non-uniform: amd64 ships .zip, arm64 ships .tar.gz.
    add_windows_archive(
        &mut ctx,
        "widget",
        "x86_64-pc-windows-msvc",
        "amd64",
        "widget",
        &"a".repeat(64),
    );
    add_windows_targz_archive(
        &mut ctx,
        "widget",
        "aarch64-pc-windows-msvc",
        "arm64",
        "widget",
        &"b".repeat(64),
    );
    let err = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect_err("ArtifactExt-in-suffix with non-uniform assets must hard-fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("widget") && msg.contains("name_template"),
        "error must name the crate + the offending field, got: {msg}"
    );
    assert!(
        msg.contains("ArtifactExt"),
        "error must call out the asset-extension embedding, got: {msg}"
    );
}

/// Mirror: the SAME `{{ ArtifactExt }}`-in-suffix template is sound when
/// every windows asset shares the `.zip` extension — the static suffix
/// resolves to a concrete `zip.sha256` (the sentinel ext token must never
/// leak into the emitted manifest).
#[test]
fn render_scoop_autoupdate_sidecar_artifactext_in_suffix_uniform_zip_ok() {
    use anodizer_core::config::ChecksumConfig;
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    c.checksum = Some(ChecksumConfig {
        split: Some(true),
        name_template: Some("{{ ArtifactName }}.{{ ArtifactExt }}.sha256".to_string()),
        ..Default::default()
    });
    let mut ctx = build_ctx(vec![c], "1.0.0");
    add_wrapping_windows_archive(&mut ctx, "widget", "widget", "1.0.0", &"d".repeat(64));
    let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(json["autoupdate"]["hash"]["url"], "$url.zip.sha256");
}

// -----------------------------------------------------------------
// is_scoop_windows_artifact / ScoopArtifactFilters / crate_has_scoop
// -----------------------------------------------------------------

fn artifact_with(target: Option<&str>, path: &str, meta: &[(&str, &str)]) -> Artifact {
    let mut m = HashMap::new();
    for (k, v) in meta {
        m.insert((*k).to_string(), (*v).to_string());
    }
    Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(path),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        target: target.map(str::to_string),
        crate_name: "widget".to_string(),
        metadata: m,
        size: None,
    }
}

/// Windows is detected by the target triple OR by the artifact path —
/// either alone suffices, and a non-Windows artifact is rejected.
#[test]
fn is_scoop_windows_artifact_by_target_or_path() {
    assert!(is_scoop_windows_artifact(&artifact_with(
        Some("x86_64-pc-windows-msvc"),
        "/dist/w-amd64.zip",
        &[]
    )));
    // No windows in the target, but the path carries it.
    assert!(is_scoop_windows_artifact(&artifact_with(
        Some("x86_64-unknown-linux-gnu"),
        "/dist/widget-windows-amd64.zip",
        &[]
    )));
    // Neither target nor path mentions windows → not a scoop artifact.
    assert!(!is_scoop_windows_artifact(&artifact_with(
        Some("x86_64-unknown-linux-gnu"),
        "/dist/widget-linux-amd64.tar.gz",
        &[]
    )));
    // Absent target falls back to the path check (no windows here).
    assert!(!is_scoop_windows_artifact(&artifact_with(
        None,
        "/dist/widget-linux.tar.gz",
        &[]
    )));
}

/// A universal binary that did NOT replace single-arch variants
/// (`replaces=false`) is filtered out before the Windows check — the
/// `only_replacing_unibins` guard.
#[test]
fn scoop_filters_reject_non_replacing_unibin() {
    let cfg = ScoopConfig::default();
    let filters = ScoopArtifactFilters::from_config(&cfg);
    let a = artifact_with(
        Some("x86_64-pc-windows-msvc"),
        "/dist/w.zip",
        &[("replaces", "false")],
    );
    assert!(
        !filters.matches(&a),
        "a non-replacing universal binary must be excluded"
    );
}

/// The `amd64_variant` filter (default `v1`) drops an amd64 Windows
/// artifact whose recorded variant differs, and keeps a matching one.
#[test]
fn scoop_filters_amd64_variant_default_v1() {
    let cfg = ScoopConfig::default(); // amd64_variant unset → defaults to v1
    let filters = ScoopArtifactFilters::from_config(&cfg);
    let v3 = artifact_with(
        Some("x86_64-pc-windows-msvc"),
        "/dist/w.zip",
        &[("amd64_variant", "v3")],
    );
    assert!(
        !filters.matches(&v3),
        "amd64_variant=v3 must be filtered when default v1 is wanted"
    );
    let v1 = artifact_with(
        Some("x86_64-pc-windows-msvc"),
        "/dist/w.zip",
        &[("amd64_variant", "v1")],
    );
    assert!(filters.matches(&v1), "amd64_variant=v1 must match default");
}

/// The `ids` allow-list filters by the artifact's `id` metadata: an
/// artifact whose id is not in the list is excluded.
#[test]
fn scoop_filters_ids_allowlist() {
    let cfg = ScoopConfig {
        ids: Some(vec!["wanted".to_string()]),
        ..Default::default()
    };
    let filters = ScoopArtifactFilters::from_config(&cfg);
    let included = artifact_with(
        Some("x86_64-pc-windows-msvc"),
        "/dist/w.zip",
        &[("id", "wanted")],
    );
    let excluded = artifact_with(
        Some("x86_64-pc-windows-msvc"),
        "/dist/w.zip",
        &[("id", "other")],
    );
    assert!(filters.matches(&included), "id 'wanted' must match");
    assert!(!filters.matches(&excluded), "id 'other' must be excluded");
}

/// `crate_has_scoop_artifacts` is false on an empty set and true once an
/// eligible Windows archive exists — the offline validator's skip signal.
#[test]
fn crate_has_scoop_artifacts_reflects_presence() {
    let c = scoop_crate_for_bucket("widget", "/unused");
    let scoop_cfg = c
        .publish
        .as_ref()
        .and_then(|p| p.scoop.clone())
        .expect("scoop cfg");
    let mut ctx = build_ctx(vec![c], "1.0.0");
    assert!(
        !crate_has_scoop_artifacts(&ctx, "widget", &scoop_cfg),
        "no windows archive => not eligible"
    );
    add_windows_archive(
        &mut ctx,
        "widget",
        "x86_64-pc-windows-msvc",
        "amd64",
        "widget",
        &"a".repeat(64),
    );
    assert!(
        crate_has_scoop_artifacts(&ctx, "widget", &scoop_cfg),
        "one windows archive => eligible"
    );
}

// -----------------------------------------------------------------
// render_scoop_manifest_for_crate — render/skip/error boundaries.
// -----------------------------------------------------------------

/// `skip_upload: true` short-circuits the renderer to `None` (the
/// publisher renders nothing for this crate) BEFORE the no-artifact
/// guard — there are no artifacts here, yet the result is `Ok(None)`.
#[test]
fn render_scoop_skip_upload_true_returns_none() {
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
        s.skip_upload = Some(StringOrBool::Bool(true));
    }
    let ctx = build_ctx(vec![c], "1.0.0");
    let out = render_scoop_manifest_for_crate(&ctx, "widget", &quiet()).expect("render ok");
    assert!(out.is_none(), "skip_upload=true must render nothing");
}

/// A falsy `if:` condition short-circuits the renderer to `None`.
#[test]
fn render_scoop_falsy_if_returns_none() {
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
        s.if_condition = Some("false".to_string());
    }
    let ctx = build_ctx(vec![c], "1.0.0");
    let out = render_scoop_manifest_for_crate(&ctx, "widget", &quiet()).expect("render ok");
    assert!(out.is_none(), "falsy `if` must render nothing");
}

/// No Windows archive → hard error naming the crate.
#[test]
fn render_scoop_no_windows_artifact_bails() {
    let c = scoop_crate_for_bucket("widget", "/unused");
    let ctx = build_ctx(vec![c], "1.0.0");
    let err = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect_err("no windows archive must bail");
    let msg = format!("{err:#}");
    assert!(msg.contains("no Windows archive artifact"), "got: {msg}");
    assert!(msg.contains("widget"), "must name the crate: {msg}");
}

/// The rendered manifest embeds the artifact's real sha256, the
/// metadata-`url`, the `bin` derived from the `binary` metadata, the
/// release-github homepage slug, and the configured license — the full
/// metadata→manifest plumbing.
#[test]
fn render_scoop_embeds_real_metadata() {
    let c = scoop_crate_for_bucket("widget", "/unused");
    let mut ctx = build_ctx(vec![c], "1.0.0");
    let sha = "b".repeat(64);
    add_windows_archive(
        &mut ctx,
        "widget",
        "x86_64-pc-windows-msvc",
        "amd64",
        "widget",
        &sha,
    );
    let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");
    let json: serde_json::Value = serde_json::from_str(&manifest).expect("valid JSON");
    assert_eq!(json["version"], "1.0.0");
    assert_eq!(json["description"], "Manage widgets from Windows");
    assert_eq!(json["license"], "MIT");
    assert_eq!(json["homepage"], "https://github.com/acme/widget");
    assert_eq!(json["architecture"]["64bit"]["hash"], sha);
    assert_eq!(
        json["architecture"]["64bit"]["url"],
        "https://github.com/acme/widget/releases/download/v1.0.0/widget-windows-amd64.zip"
    );
    assert_eq!(
        json["architecture"]["64bit"]["bin"],
        serde_json::json!(["widget.exe"]),
        "bin must derive from the `binary` metadata + .exe suffix"
    );
}

/// An artifact set including an architecture scoop cannot represent
/// (riscv64) must NOT be relabeled as `64bit` (which would have scoop
/// download an incompatible archive) and must NOT hard-fail the whole
/// manifest (which would block the valid x86_64 entry). Instead the riscv64
/// entry is warn-and-skipped: it is omitted from `architecture` and from
/// `autoupdate.architecture`, while the known arches still render correctly.
#[test]
fn render_scoop_unrepresentable_arch_warn_skipped() {
    let c = scoop_crate_for_bucket("widget", "/unused");
    let mut ctx = build_ctx(vec![c], "1.0.0");
    let sha_amd = "a".repeat(64);
    let sha_arm = "b".repeat(64);
    let sha_riscv = "c".repeat(64);
    add_windows_archive(
        &mut ctx,
        "widget",
        "x86_64-pc-windows-msvc",
        "amd64",
        "widget",
        &sha_amd,
    );
    add_windows_archive(
        &mut ctx,
        "widget",
        "aarch64-pc-windows-msvc",
        "arm64",
        "widget",
        &sha_arm,
    );
    // riscv64: scoop has no architecture key for it.
    add_windows_archive(
        &mut ctx,
        "widget",
        "riscv64gc-unknown-linux-gnu",
        "riscv64",
        "widget",
        &sha_riscv,
    );

    let capture = anodizer_core::log::LogCapture::new();
    ctx.with_log_capture(capture.clone());
    let log = ctx.logger("publish");

    let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &log)
        .expect("render ok")
        .expect("not skipped");
    let json: serde_json::Value = serde_json::from_str(&manifest).expect("valid JSON");

    // Known arches render with their real hashes.
    assert_eq!(json["architecture"]["64bit"]["hash"], sha_amd);
    assert_eq!(json["architecture"]["arm64"]["hash"], sha_arm);

    // The unrepresentable arch is omitted entirely — never relabeled as
    // 64bit (the amd64 hash, not the riscv hash, must own the 64bit slot).
    assert_ne!(
        json["architecture"]["64bit"]["hash"], sha_riscv,
        "riscv64 must not be relabeled into the 64bit slot:\n{json}"
    );
    let arch_keys: Vec<&str> = json["architecture"]
        .as_object()
        .expect("architecture object")
        .keys()
        .map(|s| s.as_str())
        .collect();
    assert!(
        !arch_keys.contains(&"riscv64"),
        "no riscv64 architecture key may be emitted, got: {arch_keys:?}"
    );

    // No dangling autoupdate/extract_dir reference for the skipped arch.
    let manifest_str = manifest.as_str();
    assert!(
        !manifest_str.contains("riscv64"),
        "the skipped arch must not appear anywhere in the manifest \
         (including autoupdate/extract_dir):\n{manifest_str}"
    );

    // A warning names the crate + the skipped arch + that scoop can't
    // represent it.
    assert!(
        capture.warn_messages().iter().any(|m| {
            m.contains("widget") && m.contains("riscv64") && m.contains("scoop supports only")
        }),
        "a warn must name the crate + skipped arch + scoop's supported set; got: {:?}",
        capture.warn_messages()
    );
}

/// Register a wrapping Windows archive whose URL + wrap directory both
/// embed the version (the real ripgrep/fd shape), for the extract_dir +
/// autoupdate e2e tests.
fn add_wrapping_windows_archive(
    ctx: &mut Context,
    crate_name: &str,
    binary: &str,
    version: &str,
    sha: &str,
) {
    let wrap = format!("{binary}-{version}-x86_64-pc-windows-msvc");
    let mut meta = HashMap::new();
    meta.insert(
        "url".to_string(),
        format!("https://github.com/acme/widget/releases/download/v{version}/{wrap}.zip"),
    );
    meta.insert("sha256".to_string(), sha.to_string());
    meta.insert("format".to_string(), "zip".to_string());
    meta.insert("binary".to_string(), binary.to_string());
    meta.insert("wrap_in_directory".to_string(), wrap.clone());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("/dist/{wrap}.zip")),
        name: format!("{wrap}.zip"),
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: crate_name.to_string(),
        metadata: meta,
        size: None,
    });
}

/// Single-crate: a wrapping archive yields a FLAT bin +
/// `extract_dir`, a derived `checkver: github`, and an `autoupdate` block
/// whose url/extract_dir are version-templated — matching real ripgrep/fd.
#[test]
fn render_scoop_extract_dir_checkver_autoupdate_default() {
    let c = scoop_crate_for_bucket("widget", "/unused");
    let mut ctx = build_ctx(vec![c], "1.2.3");
    add_wrapping_windows_archive(&mut ctx, "widget", "rg", "1.2.3", &"c".repeat(64));
    let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");
    let json: serde_json::Value = serde_json::from_str(&manifest).expect("valid JSON");

    // Flat bin + extract_dir.
    let arch = &json["architecture"]["64bit"];
    assert_eq!(arch["bin"], serde_json::json!(["rg.exe"]));
    assert_eq!(arch["extract_dir"], "rg-1.2.3-x86_64-pc-windows-msvc");

    // Derived checkver.
    assert_eq!(json["checkver"], "github");

    // Autoupdate with version-templated url + extract_dir, and a
    // combined-checksums hash (default mode emits checksums.txt).
    let au = &json["autoupdate"];
    assert_eq!(
        au["architecture"]["64bit"]["url"],
        "https://github.com/acme/widget/releases/download/v$version/rg-$version-x86_64-pc-windows-msvc.zip"
    );
    assert_eq!(
        au["architecture"]["64bit"]["extract_dir"],
        "rg-$version-x86_64-pc-windows-msvc"
    );
    assert_eq!(au["hash"]["regex"], "$sha256\\s+$basename");
    assert!(
        au["hash"]["url"].as_str().unwrap().contains("$version"),
        "combined checksums url must template the version, got:\n{au}"
    );
}

/// Per-crate, NO leakage: two crates in one workspace must each get
/// their OWN extract_dir + autoupdate asset url; crate A's wrap dir / asset
/// name must never appear in crate B's manifest (the recurring anodizer
/// cross-crate leakage bug family).
#[test]
fn render_scoop_per_crate_extract_dir_autoupdate_no_leakage() {
    let alpha = scoop_crate_for_bucket("alpha", "/unused");
    let beta = scoop_crate_for_bucket("beta", "/unused");
    let mut ctx = build_ctx(vec![alpha, beta], "1.0.0");
    add_wrapping_windows_archive(&mut ctx, "alpha", "alpha", "1.0.0", &"a".repeat(64));
    add_wrapping_windows_archive(&mut ctx, "beta", "beta", "1.0.0", &"b".repeat(64));

    let alpha_m = render_scoop_manifest_for_crate(&ctx, "alpha", &quiet())
        .expect("render ok")
        .expect("not skipped");
    let beta_m = render_scoop_manifest_for_crate(&ctx, "beta", &quiet())
        .expect("render ok")
        .expect("not skipped");

    // alpha carries only alpha's wrap dir + asset; never beta's.
    assert!(alpha_m.contains("alpha-1.0.0-x86_64-pc-windows-msvc"));
    assert!(
        !alpha_m.contains("beta-1.0.0-x86_64-pc-windows-msvc"),
        "alpha manifest leaked beta's asset:\n{alpha_m}"
    );
    // beta carries only beta's; never alpha's.
    assert!(beta_m.contains("beta-1.0.0-x86_64-pc-windows-msvc"));
    assert!(
        !beta_m.contains("alpha-1.0.0-x86_64-pc-windows-msvc"),
        "beta manifest leaked alpha's asset:\n{beta_m}"
    );

    // Both carry their own version-templated autoupdate extract_dir.
    let a: serde_json::Value = serde_json::from_str(&alpha_m).unwrap();
    let b: serde_json::Value = serde_json::from_str(&beta_m).unwrap();
    assert_eq!(
        a["autoupdate"]["architecture"]["64bit"]["extract_dir"],
        "alpha-$version-x86_64-pc-windows-msvc"
    );
    assert_eq!(
        b["autoupdate"]["architecture"]["64bit"]["extract_dir"],
        "beta-$version-x86_64-pc-windows-msvc"
    );
}

/// Workspace LOCKSTEP: every crate ships under one shared Version (the
/// global `Version` template var, no per-crate scoping). Each crate's
/// manifest must independently emit `checkver` + a version-templated
/// `autoupdate` block — lockstep mode must reach the same automated-update
/// readiness as single-crate / per-crate modes.
#[test]
fn render_scoop_lockstep_emits_checkver_and_autoupdate() {
    let alpha = scoop_crate_for_bucket("alpha", "/unused");
    let beta = scoop_crate_for_bucket("beta", "/unused");
    // Both crates render against the SAME global Version (1.5.0) — the
    // defining trait of workspace-lockstep mode.
    let mut ctx = build_ctx(vec![alpha, beta], "1.5.0");
    add_wrapping_windows_archive(&mut ctx, "alpha", "alpha", "1.5.0", &"a".repeat(64));
    add_wrapping_windows_archive(&mut ctx, "beta", "beta", "1.5.0", &"b".repeat(64));

    for crate_name in ["alpha", "beta"] {
        let manifest = render_scoop_manifest_for_crate(&ctx, crate_name, &quiet())
            .expect("render ok")
            .unwrap_or_else(|| panic!("{crate_name} not skipped"));
        let json: serde_json::Value = serde_json::from_str(&manifest).expect("valid JSON");

        assert_eq!(
            json["checkver"], "github",
            "lockstep {crate_name} must derive checkver:\n{manifest}"
        );
        let au = &json["autoupdate"];
        assert!(
            !au.is_null(),
            "lockstep {crate_name} must emit an autoupdate block:\n{manifest}"
        );
        assert_eq!(
            au["architecture"]["64bit"]["extract_dir"],
            format!("{crate_name}-$version-x86_64-pc-windows-msvc"),
            "lockstep {crate_name} autoupdate must template the version:\n{manifest}"
        );
        assert!(
            au["architecture"]["64bit"]["url"]
                .as_str()
                .unwrap()
                .contains("$version"),
            "lockstep {crate_name} autoupdate url must template the version:\n{manifest}"
        );
    }
}

/// Split mode: when the crate enables per-asset `.sha256` sidecars, the
/// autoupdate hash points at `$url.sha256` instead of a checksums file.
#[test]
fn render_scoop_autoupdate_sidecar_when_split() {
    use anodizer_core::config::ChecksumConfig;
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    c.checksum = Some(ChecksumConfig {
        split: Some(true),
        ..Default::default()
    });
    let mut ctx = build_ctx(vec![c], "1.0.0");
    add_wrapping_windows_archive(&mut ctx, "widget", "widget", "1.0.0", &"d".repeat(64));
    let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(json["autoupdate"]["hash"]["url"], "$url.sha256");
    assert!(
        json["autoupdate"]["hash"].get("regex").is_none(),
        "sidecar mode must not carry a checksums regex"
    );
}

/// Split mode with a non-default algorithm: the sidecar URL suffix must be
/// the EFFECTIVE algorithm (`blake3`), never a hardcoded `.sha256` — a
/// `$url.sha256` would 404 against the real `<asset>.blake3` sidecar.
#[test]
fn render_scoop_autoupdate_sidecar_uses_configured_algorithm() {
    use anodizer_core::config::ChecksumConfig;
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    c.checksum = Some(ChecksumConfig {
        split: Some(true),
        algorithm: Some("blake3".to_string()),
        ..Default::default()
    });
    let mut ctx = build_ctx(vec![c], "1.0.0");
    add_wrapping_windows_archive(&mut ctx, "widget", "widget", "1.0.0", &"d".repeat(64));
    let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(json["autoupdate"]["hash"]["url"], "$url.blake3");
}

/// Split mode where `algorithm` falls back from `defaults.checksum`: the
/// suffix still tracks the effective algorithm (`sha512`).
#[test]
fn render_scoop_autoupdate_sidecar_algorithm_from_defaults() {
    use anodizer_core::config::{ChecksumConfig, Defaults};
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    c.checksum = Some(ChecksumConfig {
        split: Some(true),
        ..Default::default()
    });
    let mut ctx = build_ctx(vec![c], "1.0.0");
    ctx.config.defaults = Some(Defaults {
        checksum: Some(ChecksumConfig {
            algorithm: Some("sha512".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    add_wrapping_windows_archive(&mut ctx, "widget", "widget", "1.0.0", &"d".repeat(64));
    let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(json["autoupdate"]["hash"]["url"], "$url.sha512");
}

/// Split mode with a custom `name_template` of the canonical
/// `{{ ArtifactName }}.<ext>` shape: the suffix is derivable, so the
/// sidecar URL resolves.
#[test]
fn render_scoop_autoupdate_sidecar_custom_template_derivable() {
    use anodizer_core::config::ChecksumConfig;
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    c.checksum = Some(ChecksumConfig {
        split: Some(true),
        name_template: Some("{{ ArtifactName }}.checksum".to_string()),
        ..Default::default()
    });
    let mut ctx = build_ctx(vec![c], "1.0.0");
    add_wrapping_windows_archive(&mut ctx, "widget", "widget", "1.0.0", &"d".repeat(64));
    let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");
    let json: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(json["autoupdate"]["hash"]["url"], "$url.checksum");
}

/// Split mode with an UNDERIVABLE custom `name_template` (the asset name is
/// not the leading segment): no `$url.<suffix>` form exists, so the render
/// HARD-FAILS rather than emit a 404-ing autoupdate URL.
#[test]
fn render_scoop_autoupdate_sidecar_underivable_template_errors() {
    use anodizer_core::config::ChecksumConfig;
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    c.checksum = Some(ChecksumConfig {
        split: Some(true),
        // Asset name embedded mid-string → not `<asset>.<suffix>`.
        name_template: Some("checksums-{{ ArtifactName }}.txt".to_string()),
        ..Default::default()
    });
    let mut ctx = build_ctx(vec![c], "1.0.0");
    add_wrapping_windows_archive(&mut ctx, "widget", "widget", "1.0.0", &"d".repeat(64));
    let err = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect_err("underivable split name_template must hard-fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("widget") && msg.contains("name_template"),
        "error must name the crate + the offending field, got: {msg}"
    );
}

/// `url_template` overrides the artifact's metadata URL in the rendered
/// manifest; the raw artifact URL must be gone. `{{ name }}` resolves to
/// the manifest name and `{{ os }}` to `windows`.
#[test]
fn render_scoop_url_template_overrides_metadata_url() {
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
        s.url_template = Some(
            "https://dl.acme.example/{{ name }}/{{ version }}/{{ os }}-{{ arch }}.zip".to_string(),
        );
    }
    let mut ctx = build_ctx(vec![c], "1.0.0");
    add_windows_archive(
        &mut ctx,
        "widget",
        "x86_64-pc-windows-msvc",
        "amd64",
        "widget",
        &"a".repeat(64),
    );
    let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");
    let json: serde_json::Value = serde_json::from_str(&manifest).expect("valid JSON");
    assert_eq!(
        json["architecture"]["64bit"]["url"],
        "https://dl.acme.example/widget/1.0.0/windows-amd64.zip",
        "url_template must rewrite the download URL"
    );
}

/// A `scoop.name` override drives both the manifest body and is rendered
/// through the template engine; the homepage falls back to it when no
/// release-github / explicit homepage is present.
#[test]
fn render_scoop_name_override_used_for_bin_fallback() {
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    // Drop release.github so the homepage falls back to the name slug,
    // and drop the binary metadata so `bin` derives from the manifest
    // name (the override).
    c.release = None;
    if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
        s.name = Some("widget-cli".to_string());
    }
    let mut ctx = build_ctx(vec![c], "1.0.0");
    // Archive with NO `binary` metadata → bin derives from manifest name.
    let mut meta = HashMap::new();
    meta.insert("url".to_string(), "https://example.com/w.zip".to_string());
    meta.insert("sha256".to_string(), "c".repeat(64));
    meta.insert("format".to_string(), "zip".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from("/dist/widget-windows-amd64.zip"),
        name: "widget-windows-amd64.zip".to_string(),
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "widget".to_string(),
        metadata: meta,
        size: None,
    });
    let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");
    let json: serde_json::Value = serde_json::from_str(&manifest).expect("valid JSON");
    assert_eq!(
        json["architecture"]["64bit"]["bin"],
        serde_json::json!(["widget-cli.exe"]),
        "no `binary` metadata → bin derives from the scoop.name override"
    );
    assert_eq!(
        json["homepage"], "https://github.com/widget-cli",
        "no release.github → homepage falls back to the name slug"
    );
}

// -----------------------------------------------------------------
// publish_to_scoop — non-e2e skip / dry-run guards.
// -----------------------------------------------------------------

/// `skip_upload: true` on the publish path returns `Ok(false)` (no push)
/// BEFORE the repository-resolution check — repository is None here, yet
/// the call succeeds rather than erroring on the missing repo.
#[test]
fn publish_scoop_skip_upload_short_circuits_before_repo_check() {
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
        s.repository = None;
        s.skip_upload = Some(StringOrBool::Bool(true));
    }
    let mut ctx = build_ctx(vec![c], "1.0.0");
    let pushed = publish_to_scoop(&mut ctx, "widget", &quiet())
        .expect("skip_upload must short-circuit before the repo-missing check");
    assert!(!pushed, "skip_upload path must report no push");
}

/// Missing repository config (and skip_upload unset) is a hard error.
#[test]
fn publish_scoop_missing_repository_bails() {
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
        s.repository = None;
    }
    let mut ctx = build_ctx(vec![c], "1.0.0");
    let err =
        publish_to_scoop(&mut ctx, "widget", &quiet()).expect_err("missing repository must bail");
    assert!(
        format!("{err:#}").contains("no repository config"),
        "got: {err:#}"
    );
}

/// dry-run short-circuits before any clone/push and reports no push.
#[test]
fn publish_scoop_dry_run_makes_no_push() {
    let c = scoop_crate_for_bucket("widget", "/unused");
    let config = Config {
        crates: vec![c],
        ..Default::default()
    };
    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set("ProjectName", "widget");
    add_windows_archive(
        &mut ctx,
        "widget",
        "x86_64-pc-windows-msvc",
        "amd64",
        "widget",
        &"a".repeat(64),
    );
    let pushed = publish_to_scoop(&mut ctx, "widget", &quiet()).expect("dry-run ok");
    assert!(!pushed, "dry-run must not push");
}

// -----------------------------------------------------------------
// publish_to_scoop — full clone→write→commit→push→PR against a local
// bare bucket repo (gated: spawns git, mutates PATH via the `gh` stub).
// -----------------------------------------------------------------

#[cfg(unix)]
mod e2e {
    use super::*;
    use anodizer_core::config::{PullRequestBaseConfig, PullRequestConfig};
    use anodizer_core::test_helpers::fake_tool::{FakeToolDir, PathGuard};
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };
    use serial_test::serial;
    use std::path::Path;
    use std::process::Command;

    fn git_ok(dir: &Path, args: &[&str]) {
        anodizer_core::test_helpers::git_test_ok(dir, args)
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        anodizer_core::test_helpers::git_test_stdout(dir, args)
    }

    /// Build a bare bucket repo with one commit on `main` (the branch the
    /// publish path's clone defaults to). Returns `(url, holder)`.
    fn init_bare_bucket() -> (String, tempfile::TempDir) {
        init_bare_bucket_with_files(&[])
    }

    /// [`init_bare_bucket`] variant seeding extra `(path, contents)` files
    /// into the initial commit (e.g. a pre-existing root-level manifest).
    fn init_bare_bucket_with_files(files: &[(&str, &str)]) -> (String, tempfile::TempDir) {
        let bare = tempfile::tempdir().expect("bare tempdir");
        let seed = tempfile::tempdir().expect("seed tempdir");
        git_ok(bare.path(), &["init", "--bare", "-b", "main"]);
        git_ok(seed.path(), &["init", "-b", "main"]);
        git_ok(seed.path(), &["config", "user.email", "t@example.invalid"]);
        git_ok(seed.path(), &["config", "user.name", "Test"]);
        git_ok(seed.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(seed.path().join("README"), "bucket\n").unwrap();
        for (path, contents) in files {
            let p = seed.path().join(path);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, contents).unwrap();
        }
        git_ok(seed.path(), &["add", "-A"]);
        git_ok(seed.path(), &["commit", "-m", "seed"]);
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["remote", "add", "origin"])
                        .arg(bare.path())
                        .current_dir(seed.path());
                    cmd
                },
                "git",
            )
            .status
            .success()
        );
        git_ok(seed.path(), &["push", "-u", "origin", "main"]);
        (bare.path().to_string_lossy().into_owned(), bare)
    }

    /// A `gh` stub that exits non-zero on `--version` so
    /// `gh_is_available()` is false → the PR transport falls to the API.
    fn gh_absent() -> (FakeToolDir, PathGuard) {
        let tools = FakeToolDir::new();
        tools.tool("gh").exit(1).install();
        let guard = tools.activate();
        (tools, guard)
    }

    /// Point the scripted responder's address at the publisher by
    /// injecting `ANODIZER_GITHUB_API_BASE` into the Context's env
    /// source. The base is per-Context, not process-global, so no env
    /// mutation and no pairing teardown is needed; PATH stays process
    /// global via the `gh_absent`/`gh_present` `PathGuard`.
    fn inject_api_base(ctx: &mut Context, addr: &std::net::SocketAddr) {
        ctx.set_env_source(
            anodizer_core::MapEnvSource::new()
                .with("ANODIZER_GITHUB_API_BASE", format!("http://{addr}")),
        );
    }

    /// Enable a PR against the bucket repo so `maybe_submit_pr` runs.
    fn enable_self_pr(c: &mut CrateConfig) {
        if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut())
            && let Some(r) = s.repository.as_mut()
        {
            r.pull_request = Some(PullRequestConfig {
                enabled: Some(true),
                base: Some(PullRequestBaseConfig {
                    // Same-repo PR base → no cross-repo fork sync against
                    // the bare repo, and the responder sees the PR POST.
                    owner: Some("acme".to_string()),
                    name: Some("scoop-bucket".to_string()),
                    branch: Some("main".to_string()),
                }),
                draft: None,
                body: None,
            });
        }
    }

    /// Full publish: clone the local bucket, write `<name>.json`, commit,
    /// push to `main`, then POST the PR via the API transport. Asserts
    /// the pushed manifest carries the real sha256 AND the PR-create POST
    /// reached the bucket repo's `/pulls`.
    #[test]
    #[serial(path_env)]
    fn publish_to_scoop_pushes_manifest_and_opens_pr() {
        let (_tools, _guard) = gh_absent();
        let (bucket_url, bare) = init_bare_bucket();
        let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/acme/scoop-bucket/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: Some(1),
        }]);

        let mut c = scoop_crate_for_bucket("widget", &bucket_url);
        enable_self_pr(&mut c);
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        let sha = "d".repeat(64);
        add_windows_archive(
            &mut ctx,
            "widget",
            "x86_64-pc-windows-msvc",
            "amd64",
            "widget",
            &sha,
        );

        let pushed = publish_to_scoop(&mut ctx, "widget", &quiet()).expect("publish ok");
        assert!(pushed, "a fresh manifest push must report pushed=true");

        // The manifest landed on main under the default `bucket/`
        // subdirectory with the real sha256.
        let manifest_in_repo = git_stdout(bare.path(), &["show", "main:bucket/widget.json"]);
        let json: serde_json::Value =
            serde_json::from_str(&manifest_in_repo).expect("pushed manifest is JSON");
        assert_eq!(json["architecture"]["64bit"]["hash"], sha);
        assert_eq!(json["version"], "1.0.0");

        // The PR-create POST hit the bucket repo upstream.
        let entries = req_log.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one PR-create POST expected");
        assert_eq!(entries[0].path, "/repos/acme/scoop-bucket/pulls");
        drop(entries);
        drop(bare);
    }

    /// With no `directory:` configured the manifest defaults into
    /// `bucket/` — scoop's `Find-BucketDirectory` resolves manifests ONLY
    /// from `bucket/` when that directory exists and from the repo root
    /// otherwise, so `bucket/` is correct for both layouts. A root-level
    /// manifest in a repo that also carries `bucket/` is invisible to
    /// scoop (`Couldn't find manifest`).
    #[test]
    #[serial(path_env)]
    fn publish_to_scoop_defaults_manifest_into_bucket_subdir() {
        let (_tools, _guard) = gh_absent();
        let (bucket_url, bare) = init_bare_bucket();
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/acme/scoop-bucket/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);

        let mut c = scoop_crate_for_bucket("widget", &bucket_url);
        enable_self_pr(&mut c);
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_archive(
            &mut ctx,
            "widget",
            "x86_64-pc-windows-msvc",
            "amd64",
            "widget",
            &"a".repeat(64),
        );

        publish_to_scoop(&mut ctx, "widget", &quiet()).expect("publish ok");
        let tree = git_stdout(bare.path(), &["ls-tree", "-r", "--name-only", "main"]);
        assert!(
            tree.lines().any(|l| l == "bucket/widget.json"),
            "manifest must default into bucket/; tree:\n{tree}"
        );
        assert!(
            !tree.lines().any(|l| l == "widget.json"),
            "no root-level manifest may ship alongside bucket/; tree:\n{tree}"
        );
    }

    /// A manifest previously published at the repo ROOT is dead weight
    /// once `bucket/` exists (scoop no longer resolves it) and contradicts
    /// the `bucket/` copy — publishing migrates it out in the same commit.
    #[test]
    #[serial(path_env)]
    fn publish_to_scoop_migrates_stale_root_manifest_into_bucket() {
        let (_tools, _guard) = gh_absent();
        let (bucket_url, bare) =
            init_bare_bucket_with_files(&[("widget.json", "{\"version\":\"0.9.0\"}\n")]);
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/acme/scoop-bucket/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);

        let mut c = scoop_crate_for_bucket("widget", &bucket_url);
        enable_self_pr(&mut c);
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_archive(
            &mut ctx,
            "widget",
            "x86_64-pc-windows-msvc",
            "amd64",
            "widget",
            &"b".repeat(64),
        );

        let pushed = publish_to_scoop(&mut ctx, "widget", &quiet()).expect("publish ok");
        assert!(pushed, "migration + new manifest must push");
        let tree = git_stdout(bare.path(), &["ls-tree", "-r", "--name-only", "main"]);
        assert!(
            tree.lines().any(|l| l == "bucket/widget.json"),
            "manifest must land in bucket/; tree:\n{tree}"
        );
        assert!(
            !tree.lines().any(|l| l == "widget.json"),
            "stale root manifest must be removed in the same commit; tree:\n{tree}"
        );
        let json: serde_json::Value = serde_json::from_str(&git_stdout(
            bare.path(),
            &["show", "main:bucket/widget.json"],
        ))
        .expect("bucket manifest is JSON");
        assert_eq!(json["version"], "1.0.0");
    }

    /// An explicit empty `directory: ""` is the escape hatch targeting
    /// the repo root (the pre-`bucket/`-default layout).
    #[test]
    #[serial(path_env)]
    fn publish_to_scoop_empty_directory_targets_repo_root() {
        let (_tools, _guard) = gh_absent();
        let (bucket_url, bare) = init_bare_bucket();
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/acme/scoop-bucket/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);

        let mut c = scoop_crate_for_bucket("widget", &bucket_url);
        enable_self_pr(&mut c);
        if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
            s.directory = Some(String::new());
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_archive(
            &mut ctx,
            "widget",
            "x86_64-pc-windows-msvc",
            "amd64",
            "widget",
            &"c".repeat(64),
        );

        publish_to_scoop(&mut ctx, "widget", &quiet()).expect("publish ok");
        let tree = git_stdout(bare.path(), &["ls-tree", "-r", "--name-only", "main"]);
        assert!(
            tree.lines().any(|l| l == "widget.json"),
            "empty directory must target the repo root; tree:\n{tree}"
        );
    }

    /// `directory:` places the manifest under a custom subdirectory of the
    /// bucket; the pushed file lands at `<dir>/<name>.json`.
    #[test]
    #[serial(path_env)]
    fn publish_to_scoop_honors_directory_subdir() {
        let (_tools, _guard) = gh_absent();
        let (bucket_url, bare) = init_bare_bucket();
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/acme/scoop-bucket/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);

        let mut c = scoop_crate_for_bucket("widget", &bucket_url);
        enable_self_pr(&mut c);
        if let Some(s) = c.publish.as_mut().and_then(|p| p.scoop.as_mut()) {
            s.directory = Some("manifests".to_string());
        }
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_windows_archive(
            &mut ctx,
            "widget",
            "x86_64-pc-windows-msvc",
            "amd64",
            "widget",
            &"e".repeat(64),
        );

        publish_to_scoop(&mut ctx, "widget", &quiet()).expect("publish ok");
        let tree = git_stdout(bare.path(), &["ls-tree", "-r", "--name-only", "main"]);
        assert!(
            tree.lines().any(|l| l == "manifests/widget.json"),
            "manifest must land under the configured subdirectory; tree:\n{tree}"
        );
    }

    /// Re-publishing the identical manifest finds an unchanged tree and
    /// reports `pushed=false` (NoChanges) — nothing to roll back.
    #[test]
    #[serial(path_env)]
    fn publish_to_scoop_idempotent_no_changes() {
        let (_tools, _guard) = gh_absent();
        let (bucket_url, bare) = init_bare_bucket();
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/acme/scoop-bucket/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);

        let sha = "f".repeat(64);
        let build = || {
            let mut c = scoop_crate_for_bucket("widget", &bucket_url);
            enable_self_pr(&mut c);
            let mut ctx = build_ctx(vec![c], "1.0.0");
            inject_api_base(&mut ctx, &addr);
            add_windows_archive(
                &mut ctx,
                "widget",
                "x86_64-pc-windows-msvc",
                "amd64",
                "widget",
                &sha,
            );
            ctx
        };

        let mut ctx1 = build();
        assert!(
            publish_to_scoop(&mut ctx1, "widget", &quiet()).expect("first publish"),
            "first publish pushes"
        );
        let mut ctx2 = build();
        assert!(
            !publish_to_scoop(&mut ctx2, "widget", &quiet()).expect("second publish"),
            "re-publishing an identical manifest must report NoChanges (pushed=false)"
        );
        drop(bare);
    }

    /// Publisher::run end-to-end with a real push records exactly one
    /// rollback target carrying the bucket repo URL + branch (the
    /// `any_pushed` evidence gate).
    #[test]
    #[serial(path_env)]
    fn scoop_publisher_run_records_rollback_target_after_push() {
        use anodizer_core::Publisher;
        let (_tools, _guard) = gh_absent();
        let (bucket_url, bare) = init_bare_bucket();
        let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/acme/scoop-bucket/pulls",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
            times: None,
        }]);

        let mut c = scoop_crate_for_bucket("widget", &bucket_url);
        enable_self_pr(&mut c);
        // `run` re-scopes each crate's version through
        // `with_published_crate_scope` → `resolve_crate_tag`, which
        // hard-errors unless a real tag matching `v{{ .Version }}` exists.
        // `hermetic_tagged_repo()` (tag `v0.1.0`) supplies one so the
        // scoped version resolves (the bucket branch is `main` either way).
        let project = crate::testing::hermetic_tagged_repo();
        let config = Config {
            crates: vec![c],
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                project_root: Some(project.path().to_path_buf()),
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "0.1.0");
        ctx.template_vars_mut().set("Tag", "v0.1.0");
        ctx.template_vars_mut().set("ProjectName", "widget");
        inject_api_base(&mut ctx, &addr);
        add_windows_archive(
            &mut ctx,
            "widget",
            "x86_64-pc-windows-msvc",
            "amd64",
            "widget",
            &"a".repeat(64),
        );

        let p = ScoopPublisher::new();
        let evidence = p.run(&mut ctx).expect("publisher.run ok");
        let targets = decode_scoop_targets(&evidence.extra);
        assert_eq!(targets.len(), 1, "one pushed bucket → one rollback target");
        assert_eq!(
            targets[0].repo_url,
            "https://github.com/acme/scoop-bucket.git"
        );
        assert_eq!(targets[0].branch.as_deref(), Some("main"));
        drop(bare);
    }
}

// -----------------------------------------------------------------------
// Template-rendering of user-supplied string fields.
//
// A value like `persist: "{{ .Tag }}"` must resolve to the concrete tag,
// never ship the literal `{{ .Tag }}`. Each helper below mutates one scoop
// field, renders the manifest through `render_scoop_manifest_for_crate`
// (the same path the live publish + offline guard use), and asserts the
// emitted JSON carries the resolved value and no residual `{{` delimiter.
// -----------------------------------------------------------------------

/// A widget crate whose scoop block is mutated by `f` (to set a field under
/// test), with a single x86_64 windows archive so the manifest renders.
fn render_scoop_with_field(f: impl FnOnce(&mut ScoopConfig)) -> serde_json::Value {
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    f(c.publish.as_mut().unwrap().scoop.as_mut().unwrap());
    // build_ctx sets Tag = v1.2.3 for "1.2.3"; the field templates resolve
    // against that, proving per-crate Tag scoping flows through.
    let mut ctx = build_ctx(vec![c], "1.2.3");
    add_windows_archive(
        &mut ctx,
        "widget",
        "x86_64-pc-windows-msvc",
        "amd64",
        "widget",
        &"a".repeat(64),
    );
    let manifest = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");
    assert!(
        !manifest.contains("{{"),
        "no residual template delimiter may survive:\n{manifest}"
    );
    serde_json::from_str(&manifest).expect("valid JSON")
}

#[test]
fn render_scoop_persist_field_is_template_rendered() {
    let json = render_scoop_with_field(|s| {
        s.persist = Some(vec!["data-{{ .Tag }}".to_string()]);
    });
    assert_eq!(json["persist"][0], "data-v1.2.3");
}

#[test]
fn render_scoop_depends_field_is_template_rendered() {
    let json = render_scoop_with_field(|s| {
        s.depends = Some(vec!["git".to_string(), "tool-{{ .Tag }}".to_string()]);
    });
    assert_eq!(json["depends"][0], "git");
    assert_eq!(json["depends"][1], "tool-v1.2.3");
}

#[test]
fn render_scoop_pre_install_field_is_template_rendered() {
    let json = render_scoop_with_field(|s| {
        s.pre_install = Some(vec![
            "Write-Host 'setup'".to_string(),
            "Write-Host '{{ .Tag }}'".to_string(),
        ]);
    });
    assert_eq!(json["pre_install"][1], "Write-Host 'v1.2.3'");
}

#[test]
fn render_scoop_post_install_field_is_template_rendered() {
    let json = render_scoop_with_field(|s| {
        s.post_install = Some(vec!["Write-Host 'done {{ .Tag }}'".to_string()]);
    });
    assert_eq!(json["post_install"][0], "Write-Host 'done v1.2.3'");
}

#[test]
fn render_scoop_shortcuts_field_is_template_rendered() {
    let json = render_scoop_with_field(|s| {
        s.shortcuts = Some(vec![vec![
            "widget.exe".to_string(),
            "Widget {{ .Tag }}".to_string(),
        ]]);
    });
    assert_eq!(json["shortcuts"][0][0], "widget.exe");
    assert_eq!(json["shortcuts"][0][1], "Widget v1.2.3");
}

/// The final-text guard is strict under the prepublish render pass: a field
/// carrying an unresolvable template (no such variable) must error there
/// rather than emit a manifest with a residual `{{ … }}` delimiter.
#[test]
fn render_scoop_strict_unresolvable_field_errors() {
    let mut c = scoop_crate_for_bucket("widget", "/unused");
    c.publish.as_mut().unwrap().scoop.as_mut().unwrap().persist =
        Some(vec!["{{ .NoSuchVariable }}".to_string()]);
    let mut ctx = build_ctx(vec![c], "1.2.3");
    ctx.set_render_strict(true);
    add_windows_archive(
        &mut ctx,
        "widget",
        "x86_64-pc-windows-msvc",
        "amd64",
        "widget",
        &"a".repeat(64),
    );
    let err = render_scoop_manifest_for_crate(&ctx, "widget", &quiet())
        .expect_err("strict render of an unresolvable field must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("scoop.persist"),
        "error must name the offending field, got: {msg}"
    );
}
