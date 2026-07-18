use super::*;

use crate::util::OsArtifact;

// -----------------------------------------------------------------------
// krew-release-bot mode selection + webhook tests
// -----------------------------------------------------------------------

/// Explicit `mode: bot` / `mode: pr-direct` force the flow and skip
/// the membership probe entirely (the probe would hit the network).
#[test]
fn explicit_mode_forces_flow_without_probe() {
    use anodizer_core::config::KrewMode;
    assert_eq!(
        detect_krew_flow(KrewMode::Bot, "anything", None).unwrap(),
        KrewFlow::BotWebhook
    );
    assert_eq!(
        detect_krew_flow(KrewMode::PrDirect, "anything", None).unwrap(),
        KrewFlow::PrDirect
    );
}

/// `auto` dispatch: definitive in-index → webhook; definitive absent
/// → fork PR; INDETERMINATE probe → loud error (never a silent
/// fork-PR fallback that krew maintainers reject).
#[test]
fn auto_probe_dispatch_errors_loudly_on_indeterminate() {
    assert_eq!(
        map_auto_probe("mytool", Some(true)).unwrap(),
        KrewFlow::BotWebhook
    );
    assert_eq!(
        map_auto_probe("mytool", Some(false)).unwrap(),
        KrewFlow::PrDirect
    );
    let err = map_auto_probe("mytool", None).unwrap_err().to_string();
    assert!(
        err.contains("could not determine krew-index membership"),
        "indeterminate probe must error: {err}"
    );
    // The hint must point at the explicit override + token remedies.
    assert!(
        err.contains("mode"),
        "error must mention the mode override: {err}"
    );
    assert!(
        err.contains("pr-direct") && err.contains("bot"),
        "error must name both explicit modes: {err}"
    );
}

/// The `ReleaseRequest` body carries the exact field names + values
/// the bot's server-side struct expects, and base64-encodes the
/// rendered manifest into `processedTemplate` (the server's `[]byte`
/// JSON field).
#[test]
fn release_request_body_construction() {
    use base64::Engine as _;
    let manifest = "apiVersion: krew.googlecontainertools.github.com/v1alpha2\nkind: Plugin\n";
    let req = KrewReleaseRequest::new(
        "v1.2.3",
        "mytool",
        "acme",
        "mytool-repo",
        "octocat",
        manifest,
    );
    let json = serde_json::to_string(&req).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["tagName"], "v1.2.3");
    assert_eq!(v["pluginName"], "mytool");
    assert_eq!(v["pluginOwner"], "acme");
    assert_eq!(v["pluginRepo"], "mytool-repo");
    assert_eq!(v["pluginReleaseActor"], "octocat");
    assert_eq!(v["templateFile"], ".krew.yaml");
    // processedTemplate must be base64 of the raw manifest bytes so
    // the bot's Go `[]byte` decoder reconstructs the exact manifest.
    let expected = base64::engine::general_purpose::STANDARD.encode(manifest.as_bytes());
    assert_eq!(v["processedTemplate"], expected);
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(v["processedTemplate"].as_str().unwrap())
        .unwrap();
    assert_eq!(String::from_utf8(decoded).unwrap(), manifest);
}

/// Webhook URL: env override wins; empty/unset falls back to the
/// hosted default.
#[test]
fn webhook_url_resolution_honors_env_override() {
    use anodizer_core::env_source::MapEnvSource;
    let default_env = MapEnvSource::new();
    assert_eq!(
        resolve_webhook_url(&default_env),
        DEFAULT_KREW_RELEASE_BOT_WEBHOOK_URL
    );

    let custom_env = MapEnvSource::new().with(
        "KREW_RELEASE_BOT_WEBHOOK_URL",
        "https://krew.internal.example/webhook",
    );
    assert_eq!(
        resolve_webhook_url(&custom_env),
        "https://krew.internal.example/webhook"
    );

    let blank_env = MapEnvSource::new().with("KREW_RELEASE_BOT_WEBHOOK_URL", "  ");
    assert_eq!(
        resolve_webhook_url(&blank_env),
        DEFAULT_KREW_RELEASE_BOT_WEBHOOK_URL
    );
}

/// The already-submitted classifier matches ONLY the bot's actual
/// duplicate-PR / clean-tree signals, and rejects every genuine
/// failure — including bodies that merely contain loose phrases like
/// `already exists`, so a future real error can't be swallowed.
#[test]
fn webhook_already_submitted_classifier() {
    // The benign signals the server emits.
    assert!(webhook_body_is_already_submitted(
        "opening pr: A pull request already exists for acme:mytool-v1.2.3"
    ));
    assert!(webhook_body_is_already_submitted(
        "opening pr: clean working tree, nothing to commit"
    ));
    assert!(webhook_body_is_already_submitted(
        "opening pr: clean working tree"
    ));

    // Genuine failures must NOT be swallowed.
    assert!(!webhook_body_is_already_submitted(
        "opening pr: failed when validating plugin spec"
    ));
    assert!(!webhook_body_is_already_submitted("internal server error"));
    // The loose arms dropped from the classifier: a bare resource
    // "already exists" or generic "up-to-date" is NOT the server's
    // duplicate-PR signal and must surface as a hard failure.
    assert!(!webhook_body_is_already_submitted(
        "opening pr: release already exists for tag v1.2.3"
    ));
    assert!(!webhook_body_is_already_submitted(
        "opening pr: branch already up-to-date with base"
    ));
}

/// HTTP 200 → success.
#[test]
fn webhook_submit_succeeds_on_200() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    let body =
        "PR \"https://github.com/kubernetes-sigs/krew-index/pull/42\" submitted successfully";
    let (addr, calls) = spawn_oneshot_http_responder(vec![Box::leak(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .into_boxed_str(),
    )]);
    let url = format!("http://{addr}/github-action-webhook");
    let req = KrewReleaseRequest::new("v1.0.0", "mytool", "acme", "repo", "octocat", "manifest");
    let log = StageLogger::new("publish", anodizer_core::log::Verbosity::Quiet);
    let r = submit_krew_release_webhook(&url, &req, "mytool", "1.0.0", &log);
    assert!(r.is_ok(), "200 must succeed: {r:?}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// A non-200 whose body signals an already-existing PR is an
/// idempotent no-op success.
#[test]
fn webhook_submit_idempotent_on_already_exists() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    let body = "opening pr: A pull request already exists for acme:mytool-v1.0.0";
    let (addr, _calls) = spawn_oneshot_http_responder(vec![Box::leak(
        format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .into_boxed_str(),
    )]);
    let url = format!("http://{addr}/github-action-webhook");
    let req = KrewReleaseRequest::new("v1.0.0", "mytool", "acme", "repo", "octocat", "manifest");
    let log = StageLogger::new("publish", anodizer_core::log::Verbosity::Quiet);
    let r = submit_krew_release_webhook(&url, &req, "mytool", "1.0.0", &log);
    assert!(
        r.is_ok(),
        "already-exists 500 must be a no-op success: {r:?}"
    );
}

/// A genuine failure (non-200, body not an already-exists signal)
/// surfaces a loud error — krew must never silently skip.
#[test]
fn webhook_submit_errors_on_genuine_failure() {
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    let body = "opening pr: failed when validating plugin spec";
    let (addr, _calls) = spawn_oneshot_http_responder(vec![Box::leak(
        format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .into_boxed_str(),
    )]);
    let url = format!("http://{addr}/github-action-webhook");
    let req = KrewReleaseRequest::new("v1.0.0", "mytool", "acme", "repo", "octocat", "manifest");
    let log = StageLogger::new("publish", anodizer_core::log::Verbosity::Quiet);
    let err = submit_krew_release_webhook(&url, &req, "mytool", "1.0.0", &log)
        .expect_err("genuine 500 must error");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("500") && chain.contains("validating plugin spec"),
        "error must surface status + body: {chain}"
    );
}

// -----------------------------------------------------------------------
// generate_manifest tests
// -----------------------------------------------------------------------

#[test]
fn test_generate_manifest_basic() {
    let manifest = generate_manifest(&KrewManifestParams {
        name: "kubectl-mytool",
        version: "1.0.0",
        homepage: "https://github.com/org/mytool",
        short_description: "A kubectl plugin",
        description: "A great kubectl plugin for managing things.",
        caveats: "",
        platforms: &[
            KrewPlatform {
                os: "linux".to_string(),
                arch: "amd64".to_string(),
                url: "https://example.com/mytool-linux-amd64.tar.gz".to_string(),
                sha256: "deadbeef".to_string(),
                bin: "kubectl-mytool".to_string(),
                files: vec![],
            },
            KrewPlatform {
                os: "darwin".to_string(),
                arch: "amd64".to_string(),
                url: "https://example.com/mytool-darwin-amd64.tar.gz".to_string(),
                sha256: "cafebabe".to_string(),
                bin: "kubectl-mytool".to_string(),
                files: vec![],
            },
        ],
    })
    .unwrap();

    // Header comment is present.
    assert!(manifest.starts_with(&format!("{}\n", crate::util::GENERATED_FILE_HEADER)));
    assert!(manifest.contains("apiVersion: krew.googlecontainertools.github.com/v1alpha2"));
    assert!(manifest.contains("kind: Plugin"));
    assert!(manifest.contains("  name: kubectl-mytool"));
    assert!(manifest.contains("version: v1.0.0"));
    assert!(manifest.contains("homepage: https://github.com/org/mytool"));
    assert!(manifest.contains("shortDescription: A kubectl plugin"));
    assert!(manifest.contains("A great kubectl plugin for managing things."));
    assert!(!manifest.contains("caveats:"));
    assert!(manifest.contains("platforms:"));
    assert!(manifest.contains("os: linux"));
    assert!(manifest.contains("arch: amd64"));
    assert!(manifest.contains("uri: https://example.com/mytool-linux-amd64.tar.gz"));
    assert!(manifest.contains("sha256: deadbeef"));
    assert!(manifest.contains("bin: kubectl-mytool"));
    assert!(manifest.contains("os: darwin"));
    assert!(manifest.contains("uri: https://example.com/mytool-darwin-amd64.tar.gz"));
    assert!(manifest.contains("sha256: cafebabe"));
}

/// The manifest `metadata.name` must carry the resolved `krew.name`
/// override, not the crate name — krew-index CI rejects a plugin whose
/// `metadata.name` disagrees with the declared plugin name / filename.
#[test]
fn manifest_name_uses_krew_name_override_not_crate_name() {
    // `resolve_plugin_name` picks the override over the crate name, and
    // renders it (here a no-op template render that returns its input).
    let plugin_name =
        resolve_plugin_name(Some("kubectl-mytool"), "mytool", |t| Ok(t.to_string())).unwrap();
    assert_eq!(plugin_name, "kubectl-mytool");

    let manifest = generate_manifest(&KrewManifestParams {
        name: &plugin_name,
        version: "1.0.0",
        homepage: "https://example.com",
        short_description: "A kubectl plugin",
        description: "desc",
        caveats: "",
        platforms: &[KrewPlatform {
            os: "linux".to_string(),
            arch: "amd64".to_string(),
            url: "https://example.com/mytool.tar.gz".to_string(),
            sha256: "deadbeef".to_string(),
            bin: "kubectl-mytool".to_string(),
            files: vec![],
        }],
    })
    .unwrap();

    // Assert the exact `metadata.name` line (two-space indent under
    // `metadata:`), not a substring that formatting could satisfy
    // elsewhere.
    assert!(
        manifest.contains("\nmetadata:\n  name: kubectl-mytool\n"),
        "metadata.name must be the krew.name override; got:\n{manifest}"
    );
}

/// With no `krew.name` override, the plugin name falls back to the crate
/// name (still rendered through the template engine).
#[test]
fn resolve_plugin_name_falls_back_to_crate_name() {
    let name = resolve_plugin_name(None, "mytool", |t| Ok(t.to_string())).unwrap();
    assert_eq!(name, "mytool");
}

/// A render failure in the plugin-name template propagates (it is not
/// swallowed into a literal-template plugin name).
#[test]
fn resolve_plugin_name_propagates_render_error() {
    let err = resolve_plugin_name(Some("{{ bad"), "mytool", |_| {
        anyhow::bail!("template parse error")
    })
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("render plugin name template"),
        "render error must be contextualized; got: {err}"
    );
}

#[test]
fn test_generate_manifest_with_caveats() {
    let manifest = generate_manifest(&KrewManifestParams {
        name: "my-plugin",
        version: "2.0.0",
        homepage: "https://example.com",
        short_description: "Plugin",
        description: "A plugin",
        caveats: "Run 'kubectl my-plugin init' after installation.",
        platforms: &[KrewPlatform {
            os: "linux".to_string(),
            arch: "amd64".to_string(),
            url: "https://example.com/plugin.tar.gz".to_string(),
            sha256: "hash".to_string(),
            bin: "kubectl-my-plugin".to_string(),
            files: vec![],
        }],
    })
    .unwrap();

    assert!(manifest.contains("caveats:"));
    assert!(manifest.contains("Run 'kubectl my-plugin init' after installation."));
}

#[test]
fn test_generate_manifest_no_homepage() {
    let manifest = generate_manifest(&KrewManifestParams {
        name: "tool",
        version: "1.0.0",
        homepage: "",
        short_description: "A tool",
        description: "desc",
        caveats: "",
        platforms: &[KrewPlatform {
            os: "linux".to_string(),
            arch: "amd64".to_string(),
            url: "https://example.com/tool.tar.gz".to_string(),
            sha256: "hash".to_string(),
            bin: "kubectl-tool".to_string(),
            files: vec![],
        }],
    })
    .unwrap();

    assert!(!manifest.contains("homepage:"));
}

#[test]
fn test_generate_manifest_multi_platform() {
    let manifest = generate_manifest(&KrewManifestParams {
        name: "multi",
        version: "1.0.0",
        homepage: "https://example.com",
        short_description: "Multi-platform plugin",
        description: "A plugin for all platforms.",
        caveats: "",
        platforms: &[
            KrewPlatform {
                os: "linux".to_string(),
                arch: "amd64".to_string(),
                url: "https://example.com/multi-linux-amd64.tar.gz".to_string(),
                sha256: "hash_linux_amd64".to_string(),
                bin: "kubectl-multi".to_string(),
                files: vec![],
            },
            KrewPlatform {
                os: "linux".to_string(),
                arch: "arm64".to_string(),
                url: "https://example.com/multi-linux-arm64.tar.gz".to_string(),
                sha256: "hash_linux_arm64".to_string(),
                bin: "kubectl-multi".to_string(),
                files: vec![],
            },
            KrewPlatform {
                os: "darwin".to_string(),
                arch: "amd64".to_string(),
                url: "https://example.com/multi-darwin-amd64.tar.gz".to_string(),
                sha256: "hash_darwin_amd64".to_string(),
                bin: "kubectl-multi".to_string(),
                files: vec![],
            },
            KrewPlatform {
                os: "darwin".to_string(),
                arch: "arm64".to_string(),
                url: "https://example.com/multi-darwin-arm64.tar.gz".to_string(),
                sha256: "hash_darwin_arm64".to_string(),
                bin: "kubectl-multi".to_string(),
                files: vec![],
            },
            KrewPlatform {
                os: "windows".to_string(),
                arch: "amd64".to_string(),
                url: "https://example.com/multi-windows-amd64.zip".to_string(),
                sha256: "hash_windows_amd64".to_string(),
                bin: "kubectl-multi".to_string(),
                files: vec![],
            },
        ],
    })
    .unwrap();

    // Count platform entries (each starts with "- selector:")
    let platform_count = manifest.matches("- selector:").count();
    assert_eq!(platform_count, 5);

    // Verify all platforms present
    assert!(manifest.contains("hash_linux_amd64"));
    assert!(manifest.contains("hash_linux_arm64"));
    assert!(manifest.contains("hash_darwin_amd64"));
    assert!(manifest.contains("hash_darwin_arm64"));
    assert!(manifest.contains("hash_windows_amd64"));
}

#[test]
fn test_generate_manifest_complete_structure() {
    let manifest = generate_manifest(&KrewManifestParams {
            name: "kubectl-anodizer",
            version: "3.2.1",
            homepage: "https://github.com/tj-smith47/anodizer",
            short_description: "Release automation as a kubectl plugin",
            description: "A comprehensive release automation tool\nfor Kubernetes-based projects.",
            caveats: "Ensure kubectl is configured before use.",
            platforms: &[KrewPlatform {
                os: "linux".to_string(),
                arch: "amd64".to_string(),
                url: "https://github.com/tj-smith47/anodizer/releases/download/v3.2.1/anodizer-3.2.1-linux-amd64.tar.gz".to_string(),
                sha256: "aabbccdd".to_string(),
                bin: "kubectl-anodizer".to_string(),
                files: vec![],
            }],
        }).unwrap();

    // Starts with header comment
    assert!(manifest.starts_with(&format!("{}\n", crate::util::GENERATED_FILE_HEADER)));

    // Verify structure order (line 0 is the header comment)
    let lines: Vec<&str> = manifest.lines().collect();
    assert_eq!(lines[0], crate::util::GENERATED_FILE_HEADER);
    assert_eq!(
        lines[1],
        "apiVersion: krew.googlecontainertools.github.com/v1alpha2"
    );
    assert_eq!(lines[2], "kind: Plugin");
    assert_eq!(lines[3], "metadata:");
    assert_eq!(lines[4], "  name: kubectl-anodizer");
    assert_eq!(lines[5], "spec:");
    assert!(lines[6].contains("version: v3.2.1"));

    // Multi-line description
    assert!(manifest.contains("A comprehensive release automation tool"));
    assert!(manifest.contains("for Kubernetes-based projects."));

    // Caveats
    assert!(manifest.contains("Ensure kubectl is configured before use."));
}

// -----------------------------------------------------------------------
// krew_arch / krew_os helper tests
// -----------------------------------------------------------------------

#[test]
fn test_krew_arch_mapping() {
    assert_eq!(krew_arch("amd64"), "amd64");
    assert_eq!(krew_arch("x86_64"), "amd64");
    assert_eq!(krew_arch("arm64"), "arm64");
    assert_eq!(krew_arch("aarch64"), "arm64");
    assert_eq!(krew_arch("unknown"), "unknown");
}

#[test]
fn test_krew_os_mapping() {
    assert_eq!(krew_os("darwin"), "darwin");
    assert_eq!(krew_os("macos"), "darwin");
    assert_eq!(krew_os("linux"), "linux");
    assert_eq!(krew_os("windows"), "windows");
    assert_eq!(krew_os("freebsd"), "freebsd");
}

// -----------------------------------------------------------------------
// publish_to_krew dry-run tests
// -----------------------------------------------------------------------

#[test]
fn test_publish_to_krew_missing_config() {
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

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    let log = StageLogger::new("publish", Verbosity::Normal);

    assert!(publish_to_krew(&mut ctx, "mytool", &log).is_err());
}

#[test]
fn test_publish_to_krew_missing_manifests_repo() {
    use anodizer_core::config::{Config, CrateConfig, KrewConfig, PublishConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            krew: Some(KrewConfig {
                repository: None, // Missing
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    let log = StageLogger::new("publish", Verbosity::Normal);

    assert!(publish_to_krew(&mut ctx, "mytool", &log).is_err());
}

// -----------------------------------------------------------------------
// artifacts_to_platforms .exe / binary-name-resolution tests
// -----------------------------------------------------------------------

fn make_os_artifact(os: &str, arch: &str, binary: Option<&str>) -> OsArtifact {
    // Synthesize a genuine triple so `krew_eligible` (triple-based) keeps
    // the artifact. Apple-but-not-macOS targets map to os="darwin"/"ios"
    // too but carry a different triple — see `krew_eligible_excludes_*`.
    let target = match os {
        "darwin" => "aarch64-apple-darwin",
        "linux" => "x86_64-unknown-linux-gnu",
        "windows" => "x86_64-pc-windows-msvc",
        _ => "",
    };
    OsArtifact {
        url: format!("https://example.com/{}-{}.tar.gz", os, arch),
        sha256: "deadbeef".into(),
        os: os.into(),
        arch: arch.into(),
        target: target.into(),
        binary: binary.map(|s| s.to_string()),
        ..Default::default()
    }
}

#[test]
fn krew_eligible_excludes_apple_non_macos() {
    let mk = |os: &str, target: &str| OsArtifact {
        os: os.into(),
        target: target.into(),
        ..Default::default()
    };
    // Genuine krew platforms stay eligible.
    assert!(krew_eligible(&mk("darwin", "aarch64-apple-darwin")));
    assert!(krew_eligible(&mk("linux", "x86_64-unknown-linux-gnu")));
    assert!(krew_eligible(&mk("windows", "x86_64-pc-windows-msvc")));
    assert!(krew_eligible(&mk("darwin", "darwin-universal")));
    // watchos/tvos map to os="darwin", ios to os="ios" — none installable.
    assert!(!krew_eligible(&mk("darwin", "aarch64-apple-watchos")));
    assert!(!krew_eligible(&mk("darwin", "aarch64-apple-tvos")));
    assert!(!krew_eligible(&mk("ios", "aarch64-apple-ios")));
    // A target-less artifact is excluded (no OS to install on).
    assert!(!krew_eligible(&mk("darwin", "")));
}

#[test]
fn test_artifacts_to_platforms_appends_exe_for_windows() {
    let arts = vec![
        make_os_artifact("linux", "amd64", Some("cfgd")),
        make_os_artifact("windows", "amd64", Some("cfgd")),
        make_os_artifact("darwin", "arm64", Some("cfgd")),
    ];
    let plats = artifacts_to_platforms(&arts, "cfgd");
    let by_os = |os: &str| plats.iter().find(|p| p.os == os).expect("missing platform");
    assert_eq!(by_os("linux").bin, "cfgd");
    assert_eq!(by_os("windows").bin, "cfgd.exe");
    assert_eq!(by_os("darwin").bin, "cfgd");
}

#[test]
fn test_artifacts_to_platforms_does_not_double_suffix_exe() {
    let arts = vec![make_os_artifact("windows", "amd64", Some("cfgd.exe"))];
    let plats = artifacts_to_platforms(&arts, "cfgd");
    assert_eq!(plats[0].bin, "cfgd.exe");
}

#[test]
fn test_artifacts_to_platforms_uses_archive_binary_name_over_default() {
    // Crate is `kubectl-cfgd` but ships binary `cfgd` — the manifest
    // must point at the in-archive name, not the crate name.
    let arts = vec![make_os_artifact("linux", "amd64", Some("cfgd"))];
    let plats = artifacts_to_platforms(&arts, "kubectl-cfgd");
    assert_eq!(plats[0].bin, "cfgd");
}

#[test]
fn test_artifacts_to_platforms_falls_back_to_default_when_binary_unset() {
    let arts = vec![make_os_artifact("linux", "amd64", None)];
    let plats = artifacts_to_platforms(&arts, "cfgd");
    assert_eq!(plats[0].bin, "cfgd");
}

/// Build an `OsArtifact` with explicit wrap-prefix + bundled file list so
/// the `files:` derivation can be exercised across flat / nested layouts.
fn make_os_artifact_full(
    os: &str,
    arch: &str,
    binary: Option<&str>,
    wrap: Option<&str>,
    archive_files: &[&str],
) -> OsArtifact {
    OsArtifact {
        binary: binary.map(str::to_string),
        wrap_in_directory: wrap.map(str::to_string),
        archive_files: archive_files.iter().map(|s| s.to_string()).collect(),
        ..make_os_artifact(os, arch, binary)
    }
}

// -----------------------------------------------------------------------
// `files:` extraction-list derivation (binary + LICENSE + README)
// -----------------------------------------------------------------------

/// Flat archive (no wrap dir): the binary `from` is just the binary name,
/// LICENSE + README are picked up from the bundled file set, all `to: "."`.
#[test]
fn derive_krew_files_flat_archive_binary_license_readme() {
    let a = make_os_artifact_full(
        "linux",
        "amd64",
        Some("cfgd"),
        None,
        &["LICENSE", "README.md"],
    );
    let files = derive_krew_files(&a, "cfgd");
    assert_eq!(
        files,
        vec![
            KrewFileEntry {
                from: "cfgd".into(),
                to: ".".into()
            },
            KrewFileEntry {
                from: "LICENSE".into(),
                to: ".".into()
            },
            KrewFileEntry {
                from: "README.md".into(),
                to: ".".into()
            },
        ]
    );
}

/// Nested archive (`wrap_in_directory`): both the binary AND the bundled
/// LICENSE/README `from` paths must carry the wrap prefix, or krew's
/// extractor cannot find them ("source binary cannot be found").
#[test]
fn derive_krew_files_nested_archive_prefixes_from_paths() {
    let a = make_os_artifact_full(
        "linux",
        "amd64",
        Some("cfgd"),
        Some("cfgd-1.0.0-linux-amd64"),
        &["cfgd-1.0.0-linux-amd64/LICENSE"],
    );
    let files = derive_krew_files(&a, "cfgd");
    assert_eq!(
        files,
        vec![
            KrewFileEntry {
                from: "cfgd-1.0.0-linux-amd64/cfgd".into(),
                to: ".".into()
            },
            KrewFileEntry {
                from: "cfgd-1.0.0-linux-amd64/LICENSE".into(),
                to: ".".into()
            },
        ]
    );
}

/// Windows `.exe` handling carries into the `files[].from` binary entry.
#[test]
fn derive_krew_files_windows_exe_in_from() {
    let a = make_os_artifact_full("windows", "amd64", Some("cfgd"), None, &["LICENSE"]);
    // The resolved bin (with `.exe`) is what artifacts_to_platforms passes in.
    let plats = artifacts_to_platforms(&[a], "cfgd");
    let win = &plats[0];
    assert_eq!(win.bin, "cfgd.exe");
    assert_eq!(win.files[0].from, "cfgd.exe");
    assert_eq!(win.files[0].to, ".");
    assert_eq!(win.files[1].from, "LICENSE");
}

/// LICENSE/README entries are GATED on actual archive presence: an archive
/// bundling no extra files yields a `files:` list with only the binary.
#[test]
fn derive_krew_files_absent_license_not_emitted() {
    let a = make_os_artifact_full("linux", "amd64", Some("cfgd"), None, &[]);
    let files = derive_krew_files(&a, "cfgd");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].from, "cfgd");
}

/// CHANGELOG (even `CHANGELOG.md`, the common case stage-archive bundles by
/// default) + non-license non-markdown bundled files are NOT pulled into the
/// krew `files:` list — only the binary, LICENSE, and non-changelog `*.md`.
#[test]
fn derive_krew_files_ignores_changelog_and_completions() {
    let a = make_os_artifact_full(
        "linux",
        "amd64",
        Some("cfgd"),
        None,
        &[
            "LICENSE",
            "CHANGELOG.md",
            "completions/cfgd.bash",
            "README.md",
        ],
    );
    let froms: Vec<String> = derive_krew_files(&a, "cfgd")
        .iter()
        .map(|f| f.from.clone())
        .collect();
    assert_eq!(froms, vec!["cfgd", "LICENSE", "README.md"]);
    assert!(
        !froms.iter().any(|f| f.contains("CHANGELOG")),
        "CHANGELOG.md must not leak into the krew files: list, got {froms:?}"
    );
}

/// `LICENSE.md` matches BOTH the license glob and the `*.md` README filter;
/// it must be emitted exactly ONCE (krew would otherwise copy it twice).
#[test]
fn derive_krew_files_license_md_emitted_once() {
    let a = make_os_artifact_full(
        "linux",
        "amd64",
        Some("cfgd"),
        None,
        &["LICENSE.md", "README.md"],
    );
    let froms: Vec<String> = derive_krew_files(&a, "cfgd")
        .iter()
        .map(|f| f.from.clone())
        .collect();
    assert_eq!(
        froms,
        vec!["cfgd", "LICENSE.md", "README.md"],
        "LICENSE.md must appear once (as the license), README.md once"
    );
    assert_eq!(
        froms.iter().filter(|f| f.as_str() == "LICENSE.md").count(),
        1,
        "LICENSE.md must not be duplicated, got {froms:?}"
    );
}

/// A dual-licensed archive shipping only `LICENSE-MIT`/`LICENSE-APACHE`
/// (the common Rust convention) must still install a krew-accepted
/// `LICENSE`: the selected file is renamed on extraction (`to: "LICENSE"`),
/// and the pick is the sorted-first candidate for determinism.
#[test]
fn derive_krew_files_dual_license_renamed_to_krew_accepted() {
    let a = make_os_artifact_full(
        "linux",
        "amd64",
        Some("cfgd"),
        None,
        &["LICENSE-MIT", "LICENSE-APACHE", "README.md"],
    );
    let files = derive_krew_files(&a, "cfgd");
    assert_eq!(
        files[1],
        KrewFileEntry {
            from: "LICENSE-APACHE".into(),
            to: "LICENSE".into(),
        },
        "non-krew-accepted license basename must be renamed to LICENSE, got {files:?}"
    );
}

/// When the archive bundles BOTH a krew-accepted name and suffixed
/// variants, the exact accepted name wins and keeps flat `to: "."`.
#[test]
fn derive_krew_files_prefers_exact_krew_accepted_name() {
    let a = make_os_artifact_full(
        "linux",
        "amd64",
        Some("cfgd"),
        None,
        &["LICENSE-MIT", "LICENSE"],
    );
    let files = derive_krew_files(&a, "cfgd");
    assert_eq!(
        files[1],
        KrewFileEntry {
            from: "LICENSE".into(),
            to: ".".into(),
        }
    );
}

/// The rename must carry through into the rendered manifest YAML so the
/// krew fileOperation actually installs a `LICENSE`.
#[test]
fn generate_manifest_renders_license_rename_to() {
    let platform = KrewPlatform {
        os: "linux".into(),
        arch: "amd64".into(),
        url: "https://example.com/cfgd.tar.gz".into(),
        sha256: "abc".into(),
        bin: "cfgd".into(),
        files: vec![
            KrewFileEntry {
                from: "cfgd".into(),
                to: ".".into(),
            },
            KrewFileEntry {
                from: "LICENSE-MIT".into(),
                to: "LICENSE".into(),
            },
        ],
    };
    let params = KrewManifestParams {
        name: "cfgd",
        version: "1.0.0",
        homepage: "",
        short_description: "test",
        description: "",
        caveats: "",
        platforms: std::slice::from_ref(&platform),
    };
    let yaml = generate_manifest(&params).unwrap();
    assert!(
        yaml.contains("from: LICENSE-MIT") && yaml.contains("to: LICENSE"),
        "manifest must express the rename fileOperation, got:\n{yaml}"
    );
}

/// LICENSE matching is case-insensitive (`license`, `LICENSE.txt`, …).
#[test]
fn derive_krew_files_license_case_insensitive() {
    let a = make_os_artifact_full("linux", "amd64", Some("cfgd"), None, &["license.txt"]);
    let files = derive_krew_files(&a, "cfgd");
    assert_eq!(files.len(), 2);
    assert_eq!(files[1].from, "license.txt");
}

// -----------------------------------------------------------------------
// shortDescription length validation
// -----------------------------------------------------------------------

/// A tagline within the krew-index norm produces no warning.
#[test]
fn short_description_within_norm_no_warning() {
    let (log, cap) = StageLogger::with_capture("publish", anodizer_core::log::Verbosity::Quiet);
    warn_if_short_description_too_long("Switch between contexts", "ctx", &log);
    assert_eq!(cap.warn_count(), 0);
}

/// A tagline exceeding ~50 chars warns loudly, naming the field, the crate,
/// and the actual length so the user can shorten it before krew-index review.
#[test]
fn short_description_too_long_warns_with_field_and_length() {
    let (log, cap) = StageLogger::with_capture("publish", anodizer_core::log::Verbosity::Quiet);
    let long = "This is an excessively long krew plugin tagline that will surely be flagged";
    assert!(long.chars().count() > KREW_SHORT_DESCRIPTION_MAX);
    warn_if_short_description_too_long(long, "mytool", &log);
    assert_eq!(cap.warn_count(), 1);
    let msg = cap.warn_messages().join("\n");
    assert!(msg.contains("shortDescription"), "names the field: {msg}");
    assert!(msg.contains("mytool"), "names the crate: {msg}");
    assert!(
        msg.contains(&long.chars().count().to_string()),
        "states the actual length: {msg}"
    );
}

/// Boundary: exactly the max length does NOT warn; one over does.
#[test]
fn short_description_boundary_is_inclusive() {
    let at = "x".repeat(KREW_SHORT_DESCRIPTION_MAX);
    let over = "x".repeat(KREW_SHORT_DESCRIPTION_MAX + 1);
    let (log, cap) = StageLogger::with_capture("publish", anodizer_core::log::Verbosity::Quiet);
    warn_if_short_description_too_long(&at, "c", &log);
    assert_eq!(cap.warn_count(), 0, "exactly max must not warn");
    warn_if_short_description_too_long(&over, "c", &log);
    assert_eq!(cap.warn_count(), 1, "one over max must warn");
}

#[test]
fn test_artifacts_to_platforms_arch_all_expands_with_correct_bin() {
    // arch=all should expand to amd64+arm64 with the same bin name on
    // both. Not a Windows test (krew doesn't use arch=all on windows
    // in practice) — just confirms the expansion path also flows
    // through resolve_bin.
    let arts = vec![make_os_artifact("darwin", "all", Some("cfgd"))];
    let plats = artifacts_to_platforms(&arts, "cfgd");
    assert_eq!(plats.len(), 2);
    assert!(plats.iter().all(|p| p.bin == "cfgd"));
    let arches: Vec<_> = plats.iter().map(|p| p.arch.as_str()).collect();
    assert!(arches.contains(&"amd64"));
    assert!(arches.contains(&"arm64"));
}

/// `krew.skip_upload: "{{ .IsSnapshot }}"` must template-expand
/// before its bool/auto/empty interpretation. On a snapshot run
/// the rendered value is `"true"` and the publish path must
/// short-circuit to `Ok(())` BEFORE the missing-repository check.
#[test]
fn krew_skip_upload_template_expands_to_true_on_snapshot() {
    use anodizer_core::config::{Config, CrateConfig, KrewConfig, PublishConfig, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let mut config = Config::default();
    config.project_name = "mytool".to_string();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            krew: Some(KrewConfig {
                // repository intentionally None — would normally
                // hard-fail with "no repository config", but the
                // skip_upload short-circuit must run BEFORE the
                // repository-missing check.
                repository: None,
                skip_upload: Some(StringOrBool::String("{{ .IsSnapshot }}".to_string())),
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
    ctx.template_vars_mut().set("IsSnapshot", "true");

    let log = StageLogger::new("publish", Verbosity::Normal);
    publish_to_krew(&mut ctx, "mytool", &log).expect(
        "skip_upload='{{ .IsSnapshot }}' on snapshot must short-circuit \
             to Ok(()) before the repository-missing check (GR cba5b9f)",
    );
}

// =====================================================================
// PUBLISH FLOW — render_krew_manifest_for_crate + publish_to_krew's
// PrDirect clone→write→commit→push→PR path and its error/classifier
// boundaries.
//
// The PrDirect end-to-end tests drive the live publish against a local
// bare git repo: `repository.git.url` points the clone at a `file`
// path (no network), and the PR-submission transport is forced onto an
// in-process scripted responder by installing a failing `gh` stub
// (so `gh_is_available()` is false) and injecting
// `ANODIZER_GITHUB_API_BASE` at the responder. These tests still mutate
// PATH (the `gh` stub), so each is `#[serial(path_env)]`.
//
// The krew publish path threads PR submission through the Context's
// injectable `EnvSource` (`maybe_submit_pr_with_env` /
// `submit_pr_via_gh_with_opts_with_env`), so the responder address is a
// per-Context value set via `inject_api_base` — not a process-global
// mutation. Tokens come from `repository.token` config.
// =====================================================================

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{
    Config, CrateConfig, GitRepoConfig, KrewConfig, KrewMode, PublishConfig, PullRequestConfig,
    ReleaseConfig, RepositoryConfig, ScmRepoConfig,
};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::test_helpers::fake_tool::FakeToolDir;
use anodizer_core::test_helpers::scripted_responder::{ScriptedRoute, spawn_scripted_responder};
use anodizer_core::test_helpers::{git_test_ok as git_ok, git_test_stdout as git_stdout};
use serial_test::serial;
use std::collections::HashMap;
use std::process::Command;

fn quiet() -> StageLogger {
    StageLogger::new("publish", Verbosity::Quiet)
}

/// Build a bare "krew-index fork" repo with one commit on `main`, the
/// branch the publish path's clone (`--depth=1`) defaults to. Returns
/// `(bare_path_string, _bare_holder)`. The PrDirect publish clones
/// this, writes `plugins/<name>.yaml`, commits a versioned branch, and
/// pushes it back here.
fn init_bare_fork() -> (String, tempfile::TempDir) {
    let bare = tempfile::tempdir().expect("bare tempdir");
    let seed = tempfile::tempdir().expect("seed tempdir");
    git_ok(bare.path(), &["init", "--bare", "-b", "main"]);
    git_ok(seed.path(), &["init", "-b", "main"]);
    git_ok(seed.path(), &["config", "user.email", "t@example.invalid"]);
    git_ok(seed.path(), &["config", "user.name", "Test"]);
    git_ok(seed.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(seed.path().join("README"), "krew-index\n").unwrap();
    git_ok(seed.path(), &["add", "README"]);
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
/// `gh_is_available()` is false → the PR transport falls to the API
/// path. Returns the guard (restores PATH + holds the env mutex for
/// the test's lifetime) + the on-disk stub holder.
fn gh_absent() -> (
    FakeToolDir,
    anodizer_core::test_helpers::fake_tool::PathGuard,
) {
    let tools = FakeToolDir::new();
    tools.tool("gh").exit(1).install();
    let guard = tools.activate();
    (tools, guard)
}

/// Point the scripted responder's address at the krew PR path by
/// injecting `ANODIZER_GITHUB_API_BASE` into the Context's env source.
/// The base is per-Context, not process-global, so no env mutation and
/// no teardown is needed; PATH stays process-global via the
/// `gh_absent`/`gh_present` `PathGuard`.
fn inject_api_base(ctx: &mut Context, addr: &std::net::SocketAddr) {
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("ANODIZER_GITHUB_API_BASE", format!("http://{addr}")),
    );
}

/// Register one archive artifact carrying the `url` / `sha256` /
/// `extra_binaries` metadata the manifest's `platforms[]` block reads.
/// Mirrors the schema-validation test helper so the manifest the live
/// publish renders is the same byte-for-byte shape.
fn add_archive(
    ctx: &mut Context,
    crate_name: &str,
    target: &str,
    os: &str,
    arch: &str,
    binary: &str,
    sha: &str,
) {
    let mut meta = HashMap::new();
    meta.insert(
        "url".to_string(),
        format!(
            "https://github.com/acme/widget/releases/download/v1.0.0/{binary}-{os}-{arch}.tar.gz"
        ),
    );
    meta.insert("sha256".to_string(), sha.to_string());
    meta.insert("format".to_string(), "tar.gz".to_string());
    meta.insert("extra_binaries".to_string(), binary.to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("/dist/{binary}-{os}-{arch}.tar.gz")),
        name: format!("{binary}-{os}-{arch}.tar.gz"),
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata: meta,
        size: None,
    });
}

/// A crate whose krew block clones from a local bare repo (`git.url`)
/// and PRs same-repo (so no cross-repo fork-sync), forcing the API
/// transport when `gh` is absent. `mode: pr-direct` skips the
/// network membership probe.
fn pr_direct_crate(crate_name: &str, plugin: &str, bare_url: &str) -> CrateConfig {
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
            krew: Some(KrewConfig {
                name: Some(plugin.to_string()),
                mode: Some(KrewMode::PrDirect),
                repository: Some(RepositoryConfig {
                    owner: Some("fork-owner".to_string()),
                    name: Some("krew-index".to_string()),
                    token: Some("ghp_test".to_string()),
                    git: Some(GitRepoConfig {
                        url: Some(bare_url.to_string()),
                        ssh_command: None,
                        private_key: None,
                    }),
                    pull_request: Some(PullRequestConfig {
                        enabled: Some(true),
                        // No `base` => upstream == fork => same-repo,
                        // no fork-sync side effect on the bare repo.
                        base: None,
                        draft: None,
                        body: None,
                    }),
                    ..Default::default()
                }),
                description: Some("A widget management kubectl plugin.".to_string()),
                short_description: Some("Manage widgets from kubectl".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
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
    ctx
}

// -----------------------------------------------------------------
// render_krew_manifest_for_crate — skip / error boundaries that the
// publish path short-circuits on before any clone.
// -----------------------------------------------------------------

/// `skip: true` short-circuits the renderer to `None` (the publisher
/// renders nothing for this crate). Asserts the gate fires BEFORE the
/// required-artifact / repository checks — there are no artifacts here,
/// yet the call is `Ok(None)`, not an error.
#[test]
fn render_manifest_skip_true_returns_none() {
    let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
        k.skip = Some(anodizer_core::config::StringOrBool::Bool(true));
    }
    let ctx = build_ctx(vec![c], "1.0.0");
    let out = render_krew_manifest_for_crate(&ctx, "widget", &quiet()).expect("render ok");
    assert!(out.is_none(), "skip=true must render nothing, got {out:?}");
}

/// A falsy `if:` condition short-circuits the renderer to `None`,
/// same as `skip` — proving the `if` gate is evaluated and honored.
#[test]
fn render_manifest_falsy_if_returns_none() {
    let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
        k.if_condition = Some("false".to_string());
    }
    let ctx = build_ctx(vec![c], "1.0.0");
    let out = render_krew_manifest_for_crate(&ctx, "widget", &quiet()).expect("render ok");
    assert!(out.is_none(), "falsy `if` must render nothing, got {out:?}");
}

/// A crate with no description anywhere (no krew.description, no
/// Cargo.toml fallback) bails with the actionable "description is not
/// set" message — the manifest's required narrative field.
#[test]
fn render_manifest_missing_description_bails() {
    let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
        k.description = None;
        k.short_description = None;
    }
    let ctx = build_ctx(vec![c], "1.0.0");
    let err = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
        .expect_err("missing description must bail");
    assert!(
        format!("{err:#}").contains("description is not set"),
        "got: {err:#}"
    );
}

/// No archive artifacts → hard error (a manifest with no real
/// platforms is unusable). The message must name the crate and point
/// at adding targets / removing the publisher.
#[test]
fn render_manifest_no_artifacts_bails() {
    let c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    let ctx = build_ctx(vec![c], "1.0.0");
    let err = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
        .expect_err("no artifacts must bail");
    let msg = format!("{err:#}");
    assert!(msg.contains("no archive artifacts"), "got: {msg}");
    assert!(msg.contains("widget"), "must name the crate: {msg}");
}

/// More than one binary in a single archive → bail (krew allows
/// exactly one binary per platform). `extra_binaries` is a
/// COMMA-separated list (`Artifact::extra_binaries` splits on `,`), so
/// two comma-joined names must trip the one-binary-per-archive guard
/// with the count in the message.
#[test]
fn render_manifest_multi_binary_archive_bails() {
    let c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    let mut ctx = build_ctx(vec![c], "1.0.0");
    let mut meta = HashMap::new();
    meta.insert(
        "url".to_string(),
        "https://example.com/widget-linux-amd64.tar.gz".to_string(),
    );
    meta.insert("sha256".to_string(), "a".repeat(64));
    meta.insert(
        "extra_binaries".to_string(),
        "kubectl-widget,kubectl-extra".to_string(),
    );
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from("/dist/widget-linux-amd64.tar.gz"),
        name: "widget-linux-amd64.tar.gz".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "widget".to_string(),
        metadata: meta,
        size: None,
    });
    let err = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
        .expect_err("multi-binary archive must bail");
    let msg = format!("{err:#}");
    assert!(msg.contains("only one binary per archive"), "got: {msg}");
    assert!(msg.contains("got 2"), "must report the count: {msg}");
}

/// The rendered manifest carries the crate's real sha256 + url from
/// the registered artifact (not a placeholder), with the resolved
/// plugin-name override in `metadata.name`. Pins the field plumbing
/// from artifact metadata → manifest YAML end-to-end.
#[test]
fn render_manifest_embeds_real_sha256_and_url() {
    let c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    let mut ctx = build_ctx(vec![c], "1.0.0");
    let sha = "b".repeat(64);
    add_archive(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &sha,
    );
    let manifest = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");
    assert!(
        manifest.contains(&format!("sha256: {sha}")),
        "manifest must embed the artifact's real sha256; got:\n{manifest}"
    );
    assert!(
            manifest.contains(
                "uri: https://github.com/acme/widget/releases/download/v1.0.0/kubectl-widget-linux-amd64.tar.gz"
            ),
            "manifest must embed the artifact url; got:\n{manifest}"
        );
    assert!(
        manifest.contains("\nmetadata:\n  name: kubectl-widget\n"),
        "metadata.name carries the krew.name override; got:\n{manifest}"
    );
    assert!(manifest.contains("version: v1.0.0"), "got:\n{manifest}");
}

/// Register an archive carrying the full layout metadata the krew `files:`
/// derivation reads: `wrap_in_directory` (nesting prefix) and `archive_files`
/// (bundled non-binary in-archive paths). Mirrors what stage-archive writes.
#[allow(clippy::too_many_arguments)]
fn add_archive_full(
    ctx: &mut Context,
    crate_name: &str,
    target: &str,
    os: &str,
    arch: &str,
    binary: &str,
    sha: &str,
    wrap: Option<&str>,
    archive_files: &[&str],
) {
    let mut meta = HashMap::new();
    meta.insert(
        "url".to_string(),
        format!(
            "https://github.com/acme/widget/releases/download/v1.0.0/{binary}-{os}-{arch}.tar.gz"
        ),
    );
    meta.insert("sha256".to_string(), sha.to_string());
    meta.insert("format".to_string(), "tar.gz".to_string());
    meta.insert("extra_binaries".to_string(), binary.to_string());
    if let Some(w) = wrap {
        meta.insert("wrap_in_directory".to_string(), w.to_string());
    }
    if !archive_files.is_empty() {
        meta.insert("archive_files".to_string(), archive_files.join(","));
    }
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("/dist/{binary}-{os}-{arch}.tar.gz")),
        name: format!("{binary}-{os}-{arch}.tar.gz"),
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata: meta,
        size: None,
    });
}

/// SINGLE-CRATE: a flat-layout linux archive + a windows archive must each
/// emit a concrete per-platform `files:` block — the binary (`.exe` on
/// windows) plus the bundled LICENSE — matching the krew-index exemplar shape
/// (`from`/`to: "."`). Asserts the exact rendered YAML, not a round-trip.
#[test]
fn render_manifest_emits_files_block_single_crate_linux_and_windows() {
    let c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    let mut ctx = build_ctx(vec![c], "1.0.0");
    let sha = "c".repeat(64);
    add_archive_full(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &sha,
        None,
        &["LICENSE"],
    );
    add_archive_full(
        &mut ctx,
        "widget",
        "x86_64-pc-windows-msvc",
        "windows",
        "amd64",
        "kubectl-widget",
        &sha,
        None,
        &["LICENSE"],
    );
    let manifest = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");

    // Linux platform: binary `from` is the flat binary name (no `.exe`),
    // followed by the LICENSE entry, both `to: "."`.
    let linux_block = "\
    bin: kubectl-widget
    files:
    - from: kubectl-widget
      to: .
    - from: LICENSE
      to: .";
    assert!(
        manifest.contains(linux_block),
        "linux files block must select binary + LICENSE; got:\n{manifest}"
    );

    // Windows platform: binary `from` carries the `.exe` suffix.
    let windows_block = "\
    bin: kubectl-widget.exe
    files:
    - from: kubectl-widget.exe
      to: .
    - from: LICENSE
      to: .";
    assert!(
        manifest.contains(windows_block),
        "windows files block must use the `.exe` binary name; got:\n{manifest}"
    );
}

/// SINGLE-CRATE nested layout: when the archive wraps its contents in a
/// top-level dir (`wrap_in_directory`), BOTH the binary and the LICENSE
/// `from` paths must carry that prefix, or krew's extractor fails to find
/// the binary on install.
#[test]
fn render_manifest_files_block_respects_nested_archive_layout() {
    let c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    let mut ctx = build_ctx(vec![c], "1.0.0");
    let sha = "d".repeat(64);
    add_archive_full(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &sha,
        Some("kubectl-widget-1.0.0"),
        &[
            "kubectl-widget-1.0.0/LICENSE",
            "kubectl-widget-1.0.0/README.md",
        ],
    );
    let manifest = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");
    let nested_block = "\
    files:
    - from: kubectl-widget-1.0.0/kubectl-widget
      to: .
    - from: kubectl-widget-1.0.0/LICENSE
      to: .
    - from: kubectl-widget-1.0.0/README.md
      to: .";
    assert!(
        manifest.contains(nested_block),
        "nested-layout files must carry the wrap prefix on every `from`; got:\n{manifest}"
    );
}

/// WORKSPACE PER-CRATE: two crates published in one run resolve their own
/// binary name AND their own `files:` list independently — no cross-crate
/// leakage. Each crate ships a distinct binary and a distinct bundled file.
#[test]
fn render_manifest_files_block_per_crate_no_cross_leakage() {
    let alpha = pr_direct_crate("alpha", "kubectl-alpha", "/unused");
    let beta = pr_direct_crate("beta", "kubectl-beta", "/unused");
    let mut ctx = build_ctx(vec![alpha, beta], "1.0.0");
    let sha = "e".repeat(64);
    // alpha ships binary `alpha-bin` and bundles only a LICENSE.
    add_archive_full(
        &mut ctx,
        "alpha",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "alpha-bin",
        &sha,
        None,
        &["LICENSE"],
    );
    // beta ships binary `beta-bin` and bundles a LICENSE + README.
    add_archive_full(
        &mut ctx,
        "beta",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "beta-bin",
        &sha,
        None,
        &["LICENSE", "README.md"],
    );

    let alpha_manifest = render_krew_manifest_for_crate(&ctx, "alpha", &quiet())
        .expect("alpha render ok")
        .expect("alpha not skipped");
    let beta_manifest = render_krew_manifest_for_crate(&ctx, "beta", &quiet())
        .expect("beta render ok")
        .expect("beta not skipped");

    // alpha: its OWN binary, its OWN (LICENSE-only) files list.
    assert!(
        alpha_manifest.contains(
            "\
    files:
    - from: alpha-bin
      to: .
    - from: LICENSE
      to: ."
        ),
        "alpha files must select alpha-bin + LICENSE; got:\n{alpha_manifest}"
    );
    assert!(
        !alpha_manifest.contains("beta-bin") && !alpha_manifest.contains("README.md"),
        "alpha manifest must not leak beta's binary or README; got:\n{alpha_manifest}"
    );

    // beta: its OWN binary, with the extra README entry alpha does not have.
    assert!(
        beta_manifest.contains(
            "\
    files:
    - from: beta-bin
      to: .
    - from: LICENSE
      to: .
    - from: README.md
      to: ."
        ),
        "beta files must select beta-bin + LICENSE + README; got:\n{beta_manifest}"
    );
    assert!(
        !beta_manifest.contains("alpha-bin"),
        "beta manifest must not leak alpha's binary; got:\n{beta_manifest}"
    );
}

// -----------------------------------------------------------------
// crate_has_krew_artifacts — eligibility predicate.
// -----------------------------------------------------------------

/// `crate_has_krew_artifacts` is true once an eligible archive exists
/// and false on an empty artifact set — the live path errors and the
/// offline validator skips on the same `false` signal.
#[test]
fn crate_has_krew_artifacts_reflects_artifact_presence() {
    let c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    let krew_cfg = c
        .publish
        .as_ref()
        .and_then(|p| p.krew.clone())
        .expect("krew cfg");
    let mut ctx = build_ctx(vec![c], "1.0.0");
    assert!(
        !crate_has_krew_artifacts(&ctx, "widget", &krew_cfg).unwrap(),
        "no archives => not eligible"
    );
    add_archive(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &"a".repeat(64),
    );
    assert!(
        crate_has_krew_artifacts(&ctx, "widget", &krew_cfg).unwrap(),
        "one archive => eligible"
    );
}

// -----------------------------------------------------------------
// publish_to_krew — PrDirect end-to-end against a local bare repo.
// -----------------------------------------------------------------

/// Full PrDirect single-crate publish: clone the (local) fork, write
/// `plugins/<plugin>.yaml`, commit a `<plugin>-v<version>` branch,
/// push it to the bare repo, then submit the PR via the API
/// transport. Asserts BOTH real side effects:
///   (1) the bare repo gained the versioned branch carrying the
///       manifest file with the crate's real sha256, and
///   (2) the PR-create POST reached the responder at the same-repo
///       `/repos/fork-owner/krew-index/pulls` with head = fork:branch.
#[cfg(unix)]
#[test]
#[serial(path_env)]
fn publish_to_krew_pr_direct_pushes_branch_and_opens_pr() {
    let (_tools, _guard) = gh_absent();
    let (bare_url, bare) = init_bare_fork();
    let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/repos/fork-owner/krew-index/pulls",
        response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
        times: Some(1),
    }]);
    let c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
    let mut ctx = build_ctx(vec![c], "1.0.0");
    inject_api_base(&mut ctx, &addr);
    let sha = "c".repeat(64);
    add_archive(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &sha,
    );

    let outcome = publish_to_krew(&mut ctx, "widget", &quiet()).expect("publish ok");
    assert!(
        outcome.pushed,
        "PrDirect publish must report a real push (drives any_pushed gate)"
    );

    // (1) The versioned branch landed in the bare repo, carrying the
    //     manifest file with the real sha256.
    let branches = git_stdout(bare.path(), &["branch", "--list"]);
    assert!(
        branches.contains("kubectl-widget-v1.0.0"),
        "publish must push the versioned branch; bare branches:\n{branches}"
    );
    let manifest_in_repo = git_stdout(
        bare.path(),
        &["show", "kubectl-widget-v1.0.0:plugins/kubectl-widget.yaml"],
    );
    assert!(
        manifest_in_repo.contains(&format!("sha256: {sha}")),
        "pushed manifest must carry the real sha256; got:\n{manifest_in_repo}"
    );
    assert!(
        manifest_in_repo.contains("name: kubectl-widget"),
        "pushed manifest metadata.name; got:\n{manifest_in_repo}"
    );

    // (2) The PR-create POST hit the same-repo upstream slug.
    let entries = req_log.lock().unwrap();
    assert_eq!(entries.len(), 1, "exactly one PR-create POST expected");
    assert_eq!(entries[0].path, "/repos/fork-owner/krew-index/pulls");
    let payload: serde_json::Value = serde_json::from_str(&entries[0].body).expect("JSON body");
    assert_eq!(
        payload["head"], "fork-owner:kubectl-widget-v1.0.0",
        "head must be fork-owner:<plugin>-v<version>"
    );
    drop(entries);
    drop(bare);
}

/// PrDirect publish when the upstream PR already exists: the API
/// transport returns 422 "already exists" and the publisher records a
/// `PendingValidation` override (so the dispatch summary tells the
/// truth instead of reporting `succeeded`). The branch push still
/// happened, so `pushed` is true.
#[cfg(unix)]
#[test]
#[serial(path_env)]
fn publish_to_krew_pr_direct_already_exists_records_pending() {
    let (_tools, _guard) = gh_absent();
    let (bare_url, bare) = init_bare_fork();
    let body = "{\"message\":\"Validation Failed\",\"errors\":[{\"message\":\"A pull request already exists for fork-owner:kubectl-widget-v1.0.0.\"}]}";
    let (addr, _req_log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/repos/fork-owner/krew-index/pulls",
        response: Box::leak(
            format!(
                "HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .into_boxed_str(),
        ),
        times: Some(1),
    }]);
    let c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
    let mut ctx = build_ctx(vec![c], "1.0.0");
    inject_api_base(&mut ctx, &addr);
    add_archive(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &"d".repeat(64),
    );

    let outcome = publish_to_krew(&mut ctx, "widget", &quiet()).expect("publish ok");
    assert!(outcome.pushed, "branch push happened before the PR call");
    let pending = ctx.take_pending_outcome();
    assert!(
        matches!(
            pending,
            Some(anodizer_core::PublisherOutcome::PendingValidation)
        ),
        "422 already-exists must record PendingValidation, got {pending:?}"
    );
    drop(bare);
}

/// Idempotent re-publish: when the bare fork already carries the exact
/// versioned branch + identical manifest, `commit_and_push_with_opts`
/// detects the unchanged tree and reports `NoChanges`, so the publish
/// outcome's `pushed` is false (nothing to roll back). The PR is not
/// re-submitted side-effect-wise; we assert the no-push outcome.
#[test]
#[serial(path_env)]
fn publish_to_krew_pr_direct_idempotent_no_changes() {
    let (_tools, _guard) = gh_absent();
    let (bare_url, bare) = init_bare_fork();
    // First publish pushes the branch.
    let (addr, _l1) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/repos/fork-owner/krew-index/pulls",
        response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
        times: None,
    }]);
    let sha = "e".repeat(64);
    let build = || {
        let c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
        let mut ctx = build_ctx(vec![c], "1.0.0");
        inject_api_base(&mut ctx, &addr);
        add_archive(
            &mut ctx,
            "widget",
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            "kubectl-widget",
            &sha,
        );
        ctx
    };

    let mut ctx1 = build();
    let first = publish_to_krew(&mut ctx1, "widget", &quiet()).expect("first publish");
    assert!(first.pushed, "first publish must push the branch");

    // Second publish renders the identical manifest onto the same
    // branch — the remote tree already matches, so no push.
    let mut ctx2 = build();
    let second = publish_to_krew(&mut ctx2, "widget", &quiet()).expect("second publish");
    assert!(
        !second.pushed,
        "re-publishing an identical manifest must report NoChanges (pushed=false)"
    );
    drop(bare);
}

/// Workspace per-crate mode: each crate renders + pushes its OWN
/// versioned branch under its OWN plugin name. Two krew crates sharing
/// one bare fork must each land a distinct `plugins/<plugin>.yaml` on a
/// distinct `<plugin>-v<version>` branch — proving the per-crate name +
/// branch resolution is not clobbered by a sibling.
#[test]
#[serial(path_env)]
fn publish_to_krew_pr_direct_workspace_per_crate_distinct_branches() {
    let (_tools, _guard) = gh_absent();
    let (bare_url, bare) = init_bare_fork();
    let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/repos/fork-owner/krew-index/pulls",
        response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
        times: None,
    }]);
    let alpha = pr_direct_crate("alpha", "kubectl-alpha", &bare_url);
    let beta = pr_direct_crate("beta", "kubectl-beta", &bare_url);
    let mut ctx = build_ctx(vec![alpha, beta], "2.3.4");
    inject_api_base(&mut ctx, &addr);
    for (cn, bin) in [("alpha", "kubectl-alpha"), ("beta", "kubectl-beta")] {
        add_archive(
            &mut ctx,
            cn,
            "x86_64-unknown-linux-gnu",
            "linux",
            "amd64",
            bin,
            &"f".repeat(64),
        );
    }

    publish_to_krew(&mut ctx, "alpha", &quiet()).expect("publish alpha");
    publish_to_krew(&mut ctx, "beta", &quiet()).expect("publish beta");

    let branches = git_stdout(bare.path(), &["branch", "--list"]);
    assert!(
        branches.contains("kubectl-alpha-v2.3.4"),
        "alpha branch missing; got:\n{branches}"
    );
    assert!(
        branches.contains("kubectl-beta-v2.3.4"),
        "beta branch missing; got:\n{branches}"
    );
    // Each branch carries only its own plugin manifest file.
    let alpha_file = git_stdout(
        bare.path(),
        &["show", "kubectl-alpha-v2.3.4:plugins/kubectl-alpha.yaml"],
    );
    assert!(alpha_file.contains("name: kubectl-alpha"), "{alpha_file}");
    let beta_file = git_stdout(
        bare.path(),
        &["show", "kubectl-beta-v2.3.4:plugins/kubectl-beta.yaml"],
    );
    assert!(beta_file.contains("name: kubectl-beta"), "{beta_file}");
    drop(bare);
}

/// `url_template` rewrites the pushed manifest's `platforms[].uri`
/// (not the raw artifact URL). The landed manifest in the bare repo
/// must carry the templated URL with `{{ name }}/{{ version }}/{{ os
/// }}-{{ arch }}` substituted — proving the override survives the
/// full render→push round-trip, not just an in-memory render.
///
/// `{{ name }}` resolves to the CRATE name (`widget`), not the krew
/// plugin-name override: `render_url_template_with_ctx` is called with
/// `crate_name` as its `name` arg (krew.rs ~815). The crate here is
/// `widget` and the plugin is `kubectl-widget`, so the two are
/// distinguishable in the rendered uri.
#[test]
#[serial(path_env)]
fn publish_to_krew_pr_direct_applies_url_template() {
    let (_tools, _guard) = gh_absent();
    let (bare_url, bare) = init_bare_fork();
    let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/repos/fork-owner/krew-index/pulls",
        response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
        times: None,
    }]);
    let mut c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
    if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
        k.url_template = Some(
            "https://dl.acme.example/{{ name }}/{{ version }}/{{ os }}-{{ arch }}.tar.gz"
                .to_string(),
        );
    }
    let mut ctx = build_ctx(vec![c], "1.0.0");
    inject_api_base(&mut ctx, &addr);
    add_archive(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &"a".repeat(64),
    );

    publish_to_krew(&mut ctx, "widget", &quiet()).expect("publish ok");
    let manifest_in_repo = git_stdout(
        bare.path(),
        &["show", "kubectl-widget-v1.0.0:plugins/kubectl-widget.yaml"],
    );
    assert!(
        manifest_in_repo.contains("uri: https://dl.acme.example/widget/1.0.0/linux-amd64.tar.gz"),
        "url_template must rewrite the pushed manifest uri ({{ name }} = \
             crate name 'widget'); got:\n{manifest_in_repo}"
    );
    // And the original (non-templated) artifact URL must be gone.
    assert!(
        !manifest_in_repo.contains("releases/download/v1.0.0/kubectl-widget-linux-amd64.tar.gz"),
        "the raw artifact URL must be replaced by the templated uri; got:\n{manifest_in_repo}"
    );
    drop(bare);
}

/// dry-run short-circuits before any clone/push: no branch lands in
/// the bare repo, and the outcome reports `pushed = false`. Guards the
/// "(dry-run) would submit …" early return from making real side
/// effects.
#[test]
fn publish_to_krew_dry_run_makes_no_push() {
    let (bare_url, bare) = init_bare_fork();
    let c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
    let mut config = Config {
        crates: vec![c],
        ..Default::default()
    };
    config.project_name = "widget".to_string();
    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    add_archive(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &"a".repeat(64),
    );
    let outcome = publish_to_krew(&mut ctx, "widget", &quiet()).expect("dry-run ok");
    assert!(!outcome.pushed, "dry-run must not push");
    let branches = git_stdout(bare.path(), &["branch", "--list"]);
    assert!(
        !branches.contains("kubectl-widget-v1.0.0"),
        "dry-run must not push a branch; bare branches:\n{branches}"
    );
    drop(bare);
}

/// With no `krew.homepage` and no Cargo.toml `meta_homepage`, the
/// manifest's `homepage:` falls back to the crate's `release.github`
/// slug — `https://github.com/<owner>/<repo>`. Pins the GitHub-slug
/// arm of the homepage-fallback chain (the crate's own repo, not the
/// krew-index fork owner).
#[test]
fn render_manifest_homepage_falls_back_to_release_github_slug() {
    let c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    // pr_direct_crate sets release.github = acme/widget and leaves
    // krew.homepage unset — exactly the GitHub-slug fallback case.
    let mut ctx = build_ctx(vec![c], "1.0.0");
    add_archive(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &"a".repeat(64),
    );
    let manifest = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");
    assert!(
        manifest.contains("homepage: https://github.com/acme/widget\n"),
        "homepage must derive from release.github slug; got:\n{manifest}"
    );
}

/// An explicit `krew.homepage` wins over the `release.github` slug
/// fallback and is template-rendered (the `{{ .Version }}` here
/// expands), so the operator override survives into the manifest.
#[test]
fn render_manifest_homepage_explicit_override_is_rendered() {
    let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
        k.homepage = Some("https://docs.example/widget/{{ .Version }}".to_string());
    }
    let mut ctx = build_ctx(vec![c], "1.0.0");
    add_archive(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &"a".repeat(64),
    );
    let manifest = render_krew_manifest_for_crate(&ctx, "widget", &quiet())
        .expect("render ok")
        .expect("not skipped");
    assert!(
        manifest.contains("homepage: https://docs.example/widget/1.0.0\n"),
        "explicit homepage must win and render the template; got:\n{manifest}"
    );
    // The release.github slug must NOT be used for the homepage line —
    // the override fully replaces it. (The slug legitimately appears in
    // the artifact `uri:`, so assert on the `homepage:` line specifically
    // rather than a blanket substring.)
    assert!(
        !manifest.contains("homepage: https://github.com/acme/widget"),
        "the slug fallback must not drive the homepage; got:\n{manifest}"
    );
}

// -----------------------------------------------------------------
// publish_to_krew — BotWebhook flow against a scripted responder.
// -----------------------------------------------------------------

/// `mode: bot` routes through the webhook flow: the publisher POSTs a
/// `ReleaseRequest` (with the rendered manifest base64'd into
/// `processedTemplate`) to the resolved webhook URL and, on HTTP 200,
/// returns `pushed = false` (the bot owns the krew-index PR — nothing
/// for anodizer to roll back). Asserts the request reached the
/// responder carrying the plugin coordinates.
#[test]
fn publish_to_krew_bot_webhook_posts_release_request() {
    let (bare_url, bare) = init_bare_fork();
    let resp_body =
        "PR \"https://github.com/kubernetes-sigs/krew-index/pull/7\" submitted successfully";
    let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/github-action-webhook",
        response: Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                resp_body.len(),
                resp_body
            )
            .into_boxed_str(),
        ),
        times: Some(1),
    }]);
    // The webhook URL is read from the env source on `ctx`
    // (`resolve_webhook_url(ctx.env_source())`), so point it at the
    // responder via the builder env (no process-env mutation needed).
    let mut c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
    if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
        k.mode = Some(KrewMode::Bot);
    }
    let config = Config {
        crates: vec![c],
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.set_env_source(anodizer_core::MapEnvSource::new().with(
        "KREW_RELEASE_BOT_WEBHOOK_URL",
        format!("http://{addr}/github-action-webhook"),
    ));
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    add_archive(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &"a".repeat(64),
    );

    let outcome = publish_to_krew(&mut ctx, "widget", &quiet()).expect("webhook publish ok");
    assert!(
        !outcome.pushed,
        "BotWebhook flow must report pushed=false (bot owns the PR)"
    );
    let entries = req_log.lock().unwrap();
    assert_eq!(entries.len(), 1, "exactly one webhook POST expected");
    let payload: serde_json::Value = serde_json::from_str(&entries[0].body).expect("JSON body");
    assert_eq!(payload["pluginName"], "kubectl-widget");
    assert_eq!(payload["tagName"], "v1.0.0");
    assert_eq!(payload["pluginOwner"], "acme");
    assert_eq!(payload["pluginRepo"], "widget");
    // The rendered manifest is base64'd into processedTemplate and
    // carries the crate's real artifact data.
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload["processedTemplate"].as_str().expect("base64 str"))
        .expect("decode");
    let manifest = String::from_utf8(decoded).expect("utf8");
    assert!(manifest.contains("name: kubectl-widget"), "{manifest}");
    assert!(manifest.contains("version: v1.0.0"), "{manifest}");
    drop(entries);
    drop(bare);
}

/// BotWebhook flow with no `release.github` owner/repo on the crate:
/// the webhook needs the plugin's GitHub repo to identify the
/// submission, so the publisher bails with an actionable error rather
/// than POSTing a mis-targeted request.
#[test]
fn publish_to_krew_bot_webhook_without_release_github_bails() {
    let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    c.release = None; // No plugin GitHub coordinates.
    if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
        k.mode = Some(KrewMode::Bot);
    }
    let mut ctx = build_ctx(vec![c], "1.0.0");
    add_archive(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &"a".repeat(64),
    );
    let err = publish_to_krew(&mut ctx, "widget", &quiet())
        .expect_err("webhook flow needs release.github");
    let msg = format!("{err:#}");
    assert!(msg.contains("release.github"), "got: {msg}");
    assert!(msg.contains("webhook"), "got: {msg}");
}

/// BotWebhook flow on a genuine server failure (HTTP 500 whose body is
/// NOT an already-submitted signal): the publisher surfaces a loud
/// error — krew must never silently skip a one-way publish.
#[test]
fn publish_to_krew_bot_webhook_genuine_failure_bails() {
    let (bare_url, bare) = init_bare_fork();
    let body = "opening pr: failed when validating plugin spec";
    let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/github-action-webhook",
        response: Box::leak(
            format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .into_boxed_str(),
        ),
        times: Some(1),
    }]);
    let mut c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
    if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
        k.mode = Some(KrewMode::Bot);
    }
    let config = Config {
        crates: vec![c],
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.set_env_source(anodizer_core::MapEnvSource::new().with(
        "KREW_RELEASE_BOT_WEBHOOK_URL",
        format!("http://{addr}/github-action-webhook"),
    ));
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    add_archive(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &"a".repeat(64),
    );
    let err = publish_to_krew(&mut ctx, "widget", &quiet()).expect_err("genuine 500 must bail");
    let msg = format!("{err:#}");
    assert!(msg.contains("500"), "got: {msg}");
    assert!(msg.contains("validating plugin spec"), "got: {msg}");
    drop(bare);
}

// -----------------------------------------------------------------
// artifacts_to_platforms — multi-platform expansion the publish path
// feeds into the manifest.
// -----------------------------------------------------------------

/// A multi-OS artifact set expands to one platform entry per OS with
/// the correct krew os/arch labels and the `.exe` suffix on Windows —
/// the shape the pushed manifest's `platforms[]` carries.
#[test]
fn artifacts_to_platforms_multi_os_labels_and_exe() {
    let arts = vec![
        make_os_artifact("linux", "amd64", Some("kubectl-widget")),
        make_os_artifact("darwin", "arm64", Some("kubectl-widget")),
        make_os_artifact("windows", "amd64", Some("kubectl-widget")),
    ];
    let plats = artifacts_to_platforms(&arts, "kubectl-widget");
    let find = |os: &str| plats.iter().find(|p| p.os == os).expect("platform");
    assert_eq!(find("linux").arch, "amd64");
    assert_eq!(find("linux").bin, "kubectl-widget");
    assert_eq!(find("darwin").arch, "arm64");
    assert_eq!(
        find("windows").bin,
        "kubectl-widget.exe",
        "windows bin must carry the .exe suffix krew needs"
    );
}

// -----------------------------------------------------------------
// generate_manifest — empty optional narrative fields are dropped.
// -----------------------------------------------------------------

/// An empty `description` is serialized as absent (no `description:`
/// key) — the `if params.description.is_empty()` → None branch. A
/// blank `caveats` is likewise dropped, while `shortDescription`
/// (always required) is still present.
#[test]
fn generate_manifest_empty_description_and_caveats_are_omitted() {
    let manifest = generate_manifest(&KrewManifestParams {
        name: "tool",
        version: "1.0.0",
        homepage: "https://example.com",
        short_description: "A tool",
        description: "",
        caveats: "",
        platforms: &[KrewPlatform {
            os: "linux".to_string(),
            arch: "amd64".to_string(),
            url: "https://example.com/tool.tar.gz".to_string(),
            sha256: "hash".to_string(),
            bin: "kubectl-tool".to_string(),
            files: vec![],
        }],
    })
    .unwrap();
    assert!(
        !manifest.contains("description:"),
        "empty description must be omitted; got:\n{manifest}"
    );
    assert!(
        !manifest.contains("caveats:"),
        "empty caveats must be omitted; got:\n{manifest}"
    );
    assert!(
        manifest.contains("shortDescription: A tool"),
        "shortDescription is always present; got:\n{manifest}"
    );
}

// -----------------------------------------------------------------
// publish_to_krew — skip / falsy-`if` short-circuits on the LIVE
// publish path (distinct from the renderer's gates), returning the
// skipped outcome before any repository resolution.
// -----------------------------------------------------------------

/// `skip: true` on the publish path returns a skipped outcome
/// (pushed=false) BEFORE the missing-repository check fires — the
/// crate here has no repository block, yet the call is `Ok`.
#[test]
fn publish_to_krew_skip_true_short_circuits_before_repo_check() {
    let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
        k.repository = None;
        k.skip = Some(anodizer_core::config::StringOrBool::Bool(true));
    }
    let mut ctx = build_ctx(vec![c], "1.0.0");
    let outcome = publish_to_krew(&mut ctx, "widget", &quiet())
        .expect("skip=true must short-circuit before the repo-missing check");
    assert!(!outcome.pushed, "skip path must report no push");
}

/// A falsy `if:` on the publish path returns a skipped outcome before
/// the missing-repository check.
#[test]
fn publish_to_krew_falsy_if_short_circuits_before_repo_check() {
    let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
        k.repository = None;
        k.if_condition = Some("false".to_string());
    }
    let mut ctx = build_ctx(vec![c], "1.0.0");
    let outcome = publish_to_krew(&mut ctx, "widget", &quiet())
        .expect("falsy `if` must short-circuit before the repo-missing check");
    assert!(!outcome.pushed, "falsy `if` path must report no push");
}

// -----------------------------------------------------------------
// KrewPublisher::run — real-push path records a rollback target.
// -----------------------------------------------------------------

/// Drive the Publisher trait's `run` end-to-end with a real PrDirect
/// push against a local bare fork. The `any_pushed` gate must populate
/// rollback evidence with exactly one target carrying the crate's
/// upstream coordinates + the `{plugin}-v{version}` branch — proving
/// `collect_krew_target` ran inside the per-crate scope.
///
/// `run` re-scopes each crate's version through
/// `with_published_crate_scope` → `resolve_crate_tag`, which hard-errors
/// unless a real release tag matching the `v{{ .Version }}` template
/// exists. `hermetic_tagged_repo()` (tag `v0.1.0`) supplies one, so the
/// scoped version resolves deterministically to `0.1.0` and the branch
/// is `<plugin>-v0.1.0`.
#[cfg(unix)]
#[test]
#[serial(path_env)]
fn krew_publisher_run_records_rollback_target_after_push() {
    use anodizer_core::Publisher;
    let (_tools, _guard) = gh_absent();
    let (bare_url, bare) = init_bare_fork();
    let (addr, _l) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/repos/fork-owner/krew-index/pulls",
        response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
        times: None,
    }]);

    let c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
    // Per-crate version resolution needs a real tag matching the
    // `v{{ .Version }}` template; the hermetic repo's `v0.1.0` supplies it.
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
    ctx.template_vars_mut().set("RawVersion", "0.1.0");
    ctx.template_vars_mut().set("Tag", "v0.1.0");
    inject_api_base(&mut ctx, &addr);
    add_archive(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &"a".repeat(64),
    );

    let p = KrewPublisher::new();
    let evidence = p.run(&mut ctx).expect("publisher.run ok");
    let targets = decode_krew_targets(&evidence.extra);
    assert_eq!(targets.len(), 1, "one pushed plugin → one rollback target");
    assert_eq!(targets[0].target, "widget");
    assert_eq!(targets[0].upstream_owner, "kubernetes-sigs");
    assert_eq!(targets[0].upstream_repo, "krew-index");
    assert_eq!(targets[0].fork_owner, "fork-owner");
    assert_eq!(
        targets[0].branch, "kubectl-widget-v0.1.0",
        "branch carries the per-crate-scoped version (v0.1.0 from the hermetic tag)"
    );
    drop(bare);
}

/// A `krew.description` template that fails to render (undefined field)
/// falls back to its raw `{{ }}` text via `render_or_warn` and lands in
/// the plugin manifest — `guard_no_unrendered` must hard-fail the real
/// PrDirect publish before any branch is pushed, naming the manifest.
#[test]
#[serial(path_env)]
fn publish_residual_description_template_errors_before_push() {
    let (_tools, _guard) = gh_absent();
    let (bare_url, bare) = init_bare_fork();
    let (addr, req_log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/repos/fork-owner/krew-index/pulls",
        response: "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}",
        times: None,
    }]);
    let mut c = pr_direct_crate("widget", "kubectl-widget", &bare_url);
    if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
        k.description = Some("{{ .NoSuchField }}".to_string());
    }
    let mut ctx = build_ctx(vec![c], "1.0.0");
    inject_api_base(&mut ctx, &addr);
    add_archive(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &"e".repeat(64),
    );

    let err = publish_to_krew(&mut ctx, "widget", &quiet())
        .expect_err("residual {{ }} in the plugin manifest must hard-fail");
    assert!(
        format!("{err:#}").contains("krew manifest"),
        "error must name the manifest label; got: {err:#}"
    );
    let branches = git_stdout(bare.path(), &["branch", "--list"]);
    assert!(
        !branches.contains("kubectl-widget-v1.0.0"),
        "a residual-delimiter bail must leave no pushed branch:\n{branches}"
    );
    assert!(
        req_log.lock().unwrap().is_empty(),
        "a residual-delimiter bail must fire no PR POST"
    );
    drop(bare);
}

/// The same residual `krew.description` template stays lenient in
/// dry-run: `publish_to_krew` early-returns before the manifest render
/// (and therefore before the guard), so the call must still report a
/// `Skipped` outcome rather than surface the residual as an error.
#[test]
fn publish_residual_description_template_dry_run_stays_lenient() {
    let mut c = pr_direct_crate("widget", "kubectl-widget", "/unused");
    if let Some(k) = c.publish.as_mut().and_then(|p| p.krew.as_mut()) {
        k.description = Some("{{ .NoSuchField }}".to_string());
    }
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
    ctx.template_vars_mut().set("RawVersion", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    add_archive(
        &mut ctx,
        "widget",
        "x86_64-unknown-linux-gnu",
        "linux",
        "amd64",
        "kubectl-widget",
        &"f".repeat(64),
    );

    let outcome = publish_to_krew(&mut ctx, "widget", &quiet())
        .expect("dry-run must stay lenient on a residual template");
    assert!(
        !outcome.pushed,
        "dry-run must report no push, regardless of the residual template"
    );
}
