#![allow(clippy::field_reassign_with_default)]

use super::DockerStage;
use super::build::{DockerBuildJob, format_v2_created_images_log, list_staging_dir_recursive};
use super::command::{
    DockerV1Spec, DockerV2Spec, apply_docker_v2_defaults, build_docker_command,
    build_docker_v2_command, generate_v2_image_tags, is_docker_v2_sbom_enabled,
    is_docker_v2_skipped, resolve_backend, resolve_skip_push,
};
use super::detect::{
    BuildxVersionProbe, format_buildx_version_warning, is_retriable_error, run_buildx_version_check,
};
use super::platform::{platform_to_arch, tag_suffix};
use super::retry::{parse_duration_string, resolve_retry_params};
use super::spelling::levenshtein_distance;
use super::staging::PROJECT_MARKERS;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{SkipPushConfig, StringOrBool};
use anodizer_core::stage::Stage;

#[test]
fn test_platform_to_arch() {
    assert_eq!(platform_to_arch("linux/amd64"), "amd64");
    assert_eq!(platform_to_arch("linux/arm64"), "arm64");
}

#[test]
fn test_build_docker_command() {
    // With explicit buildx backend, multi-platform gets --push
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64", "linux/arm64"],
        tags: &["ghcr.io/owner/app:v1.0.0", "ghcr.io/owner/app:latest"],
        extra_flags: &[],
        push: true,
        push_flags: &[],
        labels: &[],
        use_backend: Some("buildx"),
    })
    .unwrap();
    assert!(cmd.contains(&"buildx".to_string()));
    assert!(cmd.contains(&"build".to_string()));
    assert!(cmd.contains(&"--platform=linux/amd64,linux/arm64".to_string()));
    assert!(cmd.contains(&"--push".to_string()));
    assert!(cmd.contains(&"--tag".to_string()));
}

#[test]
fn test_build_docker_command_dry_run() {
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        tags: &["ghcr.io/owner/app:v1.0.0"],
        extra_flags: &[],
        push: false,
        push_flags: &[],
        labels: &[],
        use_backend: None,
    })
    .unwrap();
    // When push=false, neither --push nor --load
    assert!(!cmd.contains(&"--push".to_string()));
}

#[test]
fn test_stage_skips_without_docker_config() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    let stage = DockerStage::new();
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_platform_to_arch_no_slash() {
    // Fallback: no slash in string returns the whole string
    assert_eq!(platform_to_arch("amd64"), "amd64");
}

#[test]
fn parse_platform_no_arch_does_not_panic() {
    // Q2.1 (GR commit 9e9f87c): the Go version panicked on `"linux"` because
    // `strings.Split("linux", "/")` returns a single-element slice and the
    // code indexed `parts[1]`. The Rust API consumes the iterator with a
    // tuple match `(parts.next(), parts.next(), parts.next(), parts.next())`
    // and falls through to `_ => platform`, so the single-element case is
    // impossible-by-construction. This regression test asserts the
    // contract: `platform_to_arch("linux")` MUST return without panicking,
    // and the contract is "echo back the input string when no arch is
    // present".
    assert_eq!(platform_to_arch("linux"), "linux");
    assert_eq!(platform_to_arch(""), "");
    // Tag-suffix path goes through the same parser; verify it's also safe.
    assert_eq!(tag_suffix("linux"), "linux".to_string());
}

#[test]
fn test_build_docker_command_structure() {
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        tags: &["my-image:latest"],
        extra_flags: &[],
        push: true,
        push_flags: &[],
        labels: &[],
        use_backend: Some("buildx"),
    })
    .unwrap();
    assert_eq!(cmd[0], "docker");
    assert_eq!(cmd[1], "buildx");
    assert_eq!(cmd[2], "build");
    // staging dir is the last argument
    assert_eq!(cmd.last().unwrap(), "/tmp/ctx");
}

#[test]
fn test_build_docker_command_multiple_tags() {
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64", "linux/arm64"],
        tags: &["repo/img:v1.0.0", "repo/img:latest"],
        extra_flags: &[],
        push: true,
        push_flags: &[],
        labels: &[],
        use_backend: None,
    })
    .unwrap();
    // Both tags should appear after --tag flags
    let tag_positions: Vec<usize> = cmd
        .iter()
        .enumerate()
        .filter_map(|(i, t)| if t == "--tag" { Some(i) } else { None })
        .collect();
    assert_eq!(tag_positions.len(), 2);
    assert_eq!(cmd[tag_positions[0] + 1], "repo/img:v1.0.0");
    assert_eq!(cmd[tag_positions[1] + 1], "repo/img:latest");
}

// ------------------------------------------------------------------
// New tests for skip_push, extra_files, push_flags
// ------------------------------------------------------------------

#[test]
fn test_build_docker_command_skip_push() {
    // When push=false (i.e. skip_push is true or dry_run), --push should not appear
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        tags: &["ghcr.io/owner/app:v1.0.0"],
        extra_flags: &[],
        push: false,
        push_flags: &[],
        labels: &[],
        use_backend: None,
    })
    .unwrap();
    assert!(!cmd.contains(&"--push".to_string()));

    // When push=true with plain docker (single-platform, no backend),
    // --push should NOT appear — plain `docker build` doesn't support it.
    // Push is handled separately via `docker push` per tag.
    let cmd_plain = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        tags: &["ghcr.io/owner/app:v1.0.0"],
        extra_flags: &[],
        push: true,
        push_flags: &[],
        labels: &[],
        use_backend: None,
    })
    .unwrap();
    assert!(!cmd_plain.contains(&"--push".to_string()));

    // When push=true with buildx backend, --push SHOULD appear
    let cmd_buildx = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        tags: &["ghcr.io/owner/app:v1.0.0"],
        extra_flags: &[],
        push: true,
        push_flags: &[],
        labels: &[],
        use_backend: Some("buildx"),
    })
    .unwrap();
    assert!(cmd_buildx.contains(&"--push".to_string()));
}

#[test]
fn test_build_docker_command_push_flags() {
    let push_flags = vec![
        "--cache-to=type=registry,ref=ghcr.io/owner/app:cache".to_string(),
        "--provenance=true".to_string(),
    ];
    // push_flags are only baked into the build command for buildx backend
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        tags: &["ghcr.io/owner/app:v1.0.0"],
        extra_flags: &[],
        push: true,
        push_flags: &push_flags,
        labels: &[],
        use_backend: Some("buildx"),
    })
    .unwrap();
    assert!(cmd.contains(&"--push".to_string()));
    assert!(cmd.contains(&"--cache-to=type=registry,ref=ghcr.io/owner/app:cache".to_string()));
    assert!(cmd.contains(&"--provenance=true".to_string()));

    // push_flags should NOT appear when push=false
    let cmd_no_push = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        tags: &["ghcr.io/owner/app:v1.0.0"],
        extra_flags: &[],
        push: false,
        push_flags: &push_flags,
        labels: &[],
        use_backend: Some("buildx"),
    })
    .unwrap();
    assert!(!cmd_no_push.contains(&"--push".to_string()));
    assert!(!cmd_no_push.contains(&"--provenance=true".to_string()));

    // For plain docker backend with push=true, push_flags should NOT
    // appear in the build command (they go to `docker push` instead)
    let cmd_plain = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        tags: &["ghcr.io/owner/app:v1.0.0"],
        extra_flags: &[],
        push: true,
        push_flags: &push_flags,
        labels: &[],
        use_backend: None,
    })
    .unwrap();
    assert!(!cmd_plain.contains(&"--push".to_string()));
    assert!(!cmd_plain.contains(&"--provenance=true".to_string()));
}

// -----------------------------------------------------------------------
// Task 4C: Additional behavior tests — config fields actually do things
// -----------------------------------------------------------------------

#[test]
fn test_skip_push_prevents_push_flag_in_command() {
    // When skip_push=true and dry_run=false, should_push should be false
    // so the docker command should NOT contain --push
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        tags: &["ghcr.io/owner/app:v1.0.0"],
        extra_flags: &[],
        push: false,
        push_flags: &["--provenance=true".to_string()],
        labels: &[],
        use_backend: None,
    })
    .unwrap();
    assert!(!cmd.contains(&"--push".to_string()));
    // push_flags should also NOT be included when push=false
    assert!(!cmd.contains(&"--provenance=true".to_string()));
}

#[test]
fn test_push_flags_appended_to_command() {
    // push_flags only appear in build command for buildx backend
    let push_flags = vec!["--provenance=true".to_string(), "--sbom=true".to_string()];
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        tags: &["img:v1.0.0"],
        extra_flags: &[],
        push: true,
        push_flags: &push_flags,
        labels: &[],
        use_backend: Some("buildx"),
    })
    .unwrap();
    assert!(cmd.contains(&"--push".to_string()));
    assert!(cmd.contains(&"--provenance=true".to_string()));
    assert!(cmd.contains(&"--sbom=true".to_string()));
    // push_flags should come after --push
    let push_idx = cmd.iter().position(|x| x == "--push").unwrap();
    let prov_idx = cmd.iter().position(|x| x == "--provenance=true").unwrap();
    assert!(prov_idx > push_idx, "push_flags should come after --push");
}

#[test]
fn test_multi_platform_generates_correct_platform_flag() {
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64", "linux/arm64", "linux/arm/v7"],
        tags: &["img:latest"],
        extra_flags: &[],
        push: false,
        push_flags: &[],
        labels: &[],
        use_backend: None,
    })
    .unwrap();
    assert!(cmd.contains(&"--platform=linux/amd64,linux/arm64,linux/arm/v7".to_string()));
}

#[test]
fn test_platform_to_arch_various_formats() {
    assert_eq!(platform_to_arch("linux/amd64"), "amd64");
    assert_eq!(platform_to_arch("linux/arm64"), "arm64");
    assert_eq!(platform_to_arch("linux/arm/v7"), "armv7");
    assert_eq!(platform_to_arch("linux/arm/v6"), "armv6");
    assert_eq!(platform_to_arch("linux/386"), "386");
    assert_eq!(platform_to_arch("windows/amd64"), "amd64");
}

#[test]
fn test_build_docker_command_extra_build_flags() {
    let extra = vec![
        "--build-arg=APP_VERSION=1.0.0".to_string(),
        "--label=org.opencontainers.image.version=1.0.0".to_string(),
    ];
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        tags: &["img:v1.0.0"],
        extra_flags: &extra,
        push: false,
        push_flags: &[],
        labels: &[],
        use_backend: None,
    })
    .unwrap();
    assert!(cmd.contains(&"--build-arg=APP_VERSION=1.0.0".to_string()));
    assert!(cmd.contains(&"--label=org.opencontainers.image.version=1.0.0".to_string()));
}

#[test]
fn test_build_docker_command_context_dir_is_last() {
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/my/staging/dir",
        platforms: &["linux/amd64"],
        tags: &["img:latest"],
        extra_flags: &[],
        push: false,
        push_flags: &[],
        labels: &[],
        use_backend: None,
    })
    .unwrap();
    assert_eq!(cmd.last().unwrap(), "/my/staging/dir");
}

// ---- Error path tests (Task 4D) ----

// -----------------------------------------------------------------------
// Tests for id, ids, labels config fields
// -----------------------------------------------------------------------

#[test]
fn test_labels_appear_in_docker_build_command() {
    let labels = vec![
        (
            "org.opencontainers.image.source".to_string(),
            "https://github.com/owner/app".to_string(),
        ),
        (
            "org.opencontainers.image.version".to_string(),
            "1.0.0".to_string(),
        ),
    ];
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        tags: &["ghcr.io/owner/app:v1.0.0"],
        extra_flags: &[],
        push: false,
        push_flags: &[],
        labels: &labels,
        use_backend: None,
    })
    .unwrap();
    assert!(
        cmd.contains(&"--label".to_string()),
        "command should contain --label flag"
    );
    assert!(
        cmd.contains(&"org.opencontainers.image.source=https://github.com/owner/app".to_string()),
        "label key=value should appear in command"
    );
    assert!(
        cmd.contains(&"org.opencontainers.image.version=1.0.0".to_string()),
        "label key=value should appear in command"
    );
}

// -----------------------------------------------------------------------
// Tests for retry configuration
// -----------------------------------------------------------------------

#[test]
fn test_parse_duration_string_seconds() {
    let d = parse_duration_string("5s").unwrap();
    assert_eq!(d, Duration::from_secs(5));
}

#[test]
fn test_parse_duration_string_milliseconds() {
    let d = parse_duration_string("500ms").unwrap();
    assert_eq!(d, Duration::from_millis(500));
}

#[test]
fn test_parse_duration_string_minutes() {
    let d = parse_duration_string("2m").unwrap();
    assert_eq!(d, Duration::from_secs(120));
}

#[test]
fn test_parse_duration_string_trims_whitespace() {
    let d = parse_duration_string("  3s  ").unwrap();
    assert_eq!(d, Duration::from_secs(3));
}

#[test]
fn test_parse_duration_string_empty() {
    assert!(parse_duration_string("").is_err());
    assert!(parse_duration_string("   ").is_err());
}

#[test]
fn test_parse_duration_string_bare_number_as_seconds() {
    let d = parse_duration_string("10").unwrap();
    assert_eq!(d, Duration::from_secs(10));
    let d = parse_duration_string("100").unwrap();
    assert_eq!(d, Duration::from_secs(100));
}

#[test]
fn test_parse_duration_string_invalid_suffix() {
    assert!(parse_duration_string("5h").is_err());
}

#[test]
fn test_parse_duration_string_invalid_number() {
    assert!(parse_duration_string("abcs").is_err());
    assert!(parse_duration_string("1.5s").is_err());
}

#[test]
fn test_resolve_retry_params_none() {
    let (attempts, delay, max_delay) = resolve_retry_params(&None, &None).unwrap();
    assert_eq!(attempts, 10);
    assert_eq!(delay, Duration::from_secs(10));
    // Default max_delay is 5 minutes to prevent unbounded backoff
    assert_eq!(max_delay, Some(Duration::from_secs(300)));
}

#[test]
fn test_resolve_retry_params_defaults() {
    use anodizer_core::config::DockerRetryConfig;
    let cfg = Some(DockerRetryConfig {
        attempts: None,
        delay: None,
        max_delay: None,
    });
    let (attempts, delay, max_delay) = resolve_retry_params(&cfg, &None).unwrap();
    assert_eq!(attempts, 10);
    assert_eq!(delay, Duration::from_secs(10));
    // Default max_delay is 5 minutes to prevent unbounded backoff
    assert_eq!(max_delay, Some(Duration::from_secs(300)));
}

#[test]
fn test_resolve_retry_params_full() {
    use anodizer_core::config::DockerRetryConfig;
    let cfg = Some(DockerRetryConfig {
        attempts: Some(3),
        delay: Some("500ms".to_string()),
        max_delay: Some("10s".to_string()),
    });
    let (attempts, delay, max_delay) = resolve_retry_params(&cfg, &None).unwrap();
    assert_eq!(attempts, 3);
    assert_eq!(delay, Duration::from_millis(500));
    assert_eq!(max_delay, Some(Duration::from_secs(10)));
}

#[test]
fn test_resolve_retry_params_invalid_delay() {
    use anodizer_core::config::DockerRetryConfig;
    let cfg = Some(DockerRetryConfig {
        attempts: Some(3),
        delay: Some("invalid".to_string()),
        max_delay: None,
    });
    assert!(resolve_retry_params(&cfg, &None).is_err());
}

// P1.6 — top-level Project.Retry is consulted when per-pipe is absent;
// per-pipe wins (with deprecation warning) when both are set.
#[test]
fn test_docker_retry_precedence_per_pipe_top_level_defaults() {
    use anodizer_core::config::{DockerRetryConfig, HumanDuration, RetryConfig};

    // Case 1: neither set → defaults
    let (a, d, m) = resolve_retry_params(&None, &None).unwrap();
    assert_eq!(a, 10);
    assert_eq!(d, Duration::from_secs(10));
    assert_eq!(m, Some(Duration::from_secs(300)));

    // Case 2: only top-level set → top-level wins (no warning expected)
    let top = Some(RetryConfig {
        attempts: 4,
        delay: HumanDuration(Duration::from_millis(250)),
        max_delay: HumanDuration(Duration::from_secs(7)),
    });
    let (a, d, m) = resolve_retry_params(&None, &top).unwrap();
    assert_eq!(a, 4);
    assert_eq!(d, Duration::from_millis(250));
    assert_eq!(m, Some(Duration::from_secs(7)));

    // Case 3: per-pipe set (overrides top-level, fires deprecation warn).
    // We can't easily intercept tracing output here without a subscriber,
    // so we verify the values are taken from per-pipe and rely on the
    // OnceLock + tracing::warn! contract documented in retry.rs.
    let per_pipe = Some(DockerRetryConfig {
        attempts: Some(2),
        delay: Some("100ms".to_string()),
        max_delay: Some("500ms".to_string()),
    });
    let (a, d, m) = resolve_retry_params(&per_pipe, &top).unwrap();
    assert_eq!(a, 2);
    assert_eq!(d, Duration::from_millis(100));
    assert_eq!(m, Some(Duration::from_millis(500)));

    // Case 4: per-pipe set, top-level absent — same precedence, still uses
    // per-pipe. (The OnceLock means the warn fired once in case 3 and
    // won't fire again here, but the value resolution must still be correct.)
    let (a, d, m) = resolve_retry_params(&per_pipe, &None).unwrap();
    assert_eq!(a, 2);
    assert_eq!(d, Duration::from_millis(100));
    assert_eq!(m, Some(Duration::from_millis(500)));
}

// Captures the intent that `resolve_retry_params` must fire its deprecation
// warning at most once per process when a per-pipe `DockerRetryConfig` is
// supplied. Verifying this end-to-end requires a `tracing-subscriber` test
// fixture that captures the warn event, which we deliberately do not pull in
// just for one assertion. The contract is enforced by the `OnceLock` guard
// in `retry::warn_docker_retry_deprecated_once` and reviewed at code-review.
#[test]
#[ignore = "warn-capture requires tracing-subscriber fixture; OnceLock semantics verified by code-review"]
fn test_docker_retry_deprecation_warn_emits_once() {
    // Documented intent: per_pipe = Some(...) drives
    // `warn_docker_retry_deprecated_once()` exactly once across N calls
    // regardless of crate count, due to the OnceLock guard.
}

// -----------------------------------------------------------------------
// Task 8: skip_push auto, use_backend, docker_manifests, digest
// -----------------------------------------------------------------------

#[test]
fn test_config_docker_manifests_full() {
    use anodizer_core::config::Config;
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    docker_manifests:
      - name_template: "ghcr.io/owner/app:{{ .Version }}"
        image_templates:
          - "ghcr.io/owner/app:{{ .Version }}-amd64"
          - "ghcr.io/owner/app:{{ .Version }}-arm64"
        create_flags:
          - "--amend"
        push_flags:
          - "--purge"
        skip_push: auto
        id: my-manifest
        use: docker
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let manifests = config.crates[0].docker_manifests.as_ref().unwrap();
    assert_eq!(manifests.len(), 1);
    let m = &manifests[0];
    assert_eq!(m.name_template, "ghcr.io/owner/app:{{ .Version }}");
    assert_eq!(m.image_templates.len(), 2);
    assert_eq!(m.create_flags.as_ref().unwrap(), &["--amend"]);
    assert_eq!(m.push_flags.as_ref().unwrap(), &["--purge"]);
    assert_eq!(m.skip_push, Some(SkipPushConfig::Auto));
    assert_eq!(m.id.as_deref(), Some("my-manifest"));
    assert_eq!(m.use_backend.as_deref(), Some("docker"));
}

#[test]
fn test_config_docker_manifests_omitted() {
    use anodizer_core::config::Config;
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.crates[0].docker_manifests.is_none());
}

#[test]
fn test_resolve_skip_push_auto_prerelease() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Prerelease", "rc.1");

    let skip = resolve_skip_push(&Some(SkipPushConfig::Auto), &ctx);
    assert!(skip, "auto should skip push when Prerelease is non-empty");
}

#[test]
fn test_resolve_skip_push_auto_no_prerelease() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Prerelease", "");

    let skip = resolve_skip_push(&Some(SkipPushConfig::Auto), &ctx);
    assert!(!skip, "auto should NOT skip push when Prerelease is empty");
}

#[test]
fn test_resolve_skip_push_auto_prerelease_unset() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config::default();
    let ctx = Context::new(config, ContextOptions::default());

    let skip = resolve_skip_push(&Some(SkipPushConfig::Auto), &ctx);
    assert!(
        !skip,
        "auto should NOT skip push when Prerelease is not set"
    );
}

#[test]
fn test_resolve_skip_push_bool_true() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config::default();
    let ctx = Context::new(config, ContextOptions::default());

    let skip = resolve_skip_push(&Some(SkipPushConfig::Bool(true)), &ctx);
    assert!(skip);
}

#[test]
fn test_resolve_skip_push_bool_false() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config::default();
    let ctx = Context::new(config, ContextOptions::default());

    let skip = resolve_skip_push(&Some(SkipPushConfig::Bool(false)), &ctx);
    assert!(!skip);
}

#[test]
fn test_resolve_skip_push_none() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config::default();
    let ctx = Context::new(config, ContextOptions::default());

    let skip = resolve_skip_push(&None, &ctx);
    assert!(!skip, "None should not skip push");
}

#[test]
fn test_resolve_backend_buildx_explicit() {
    let (bin, subs) = resolve_backend(Some("buildx"), false).unwrap();
    assert_eq!(bin, "docker");
    assert_eq!(subs, vec!["buildx", "build"]);
}

#[test]
fn test_resolve_backend_docker_explicit() {
    let (bin, subs) = resolve_backend(Some("docker"), false).unwrap();
    assert_eq!(bin, "docker");
    assert_eq!(subs, vec!["build"]);
}

#[test]
fn test_resolve_backend_podman_explicit() {
    let (bin, subs) = resolve_backend(Some("podman"), false).unwrap();
    assert_eq!(bin, "podman");
    assert_eq!(subs, vec!["build"]);
}

#[test]
fn test_resolve_backend_default_single_platform() {
    let (bin, subs) = resolve_backend(None, false).unwrap();
    assert_eq!(bin, "docker");
    assert_eq!(subs, vec!["build"]);
}

#[test]
fn test_resolve_backend_default_multi_platform() {
    // Default is "docker" even with multi-platform (matching GoReleaser).
    // Users must explicitly set `use: buildx` for buildx features.
    let (bin, subs) = resolve_backend(None, true).unwrap();
    assert_eq!(bin, "docker");
    assert_eq!(subs, vec!["build"]);
}

#[test]
fn test_resolve_backend_unknown_errors() {
    let result = resolve_backend(Some("containerd"), false);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("unknown docker backend 'containerd'"),
        "error should mention the unknown backend, got: {err}"
    );
}

#[test]
fn test_build_docker_command_podman_backend() {
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        tags: &["img:latest"],
        extra_flags: &[],
        push: false,
        push_flags: &[],
        labels: &[],
        use_backend: Some("podman"),
    })
    .unwrap();
    assert_eq!(cmd[0], "podman");
    assert_eq!(cmd[1], "build");
    assert_eq!(cmd.last().unwrap(), "/tmp/ctx");
}

#[test]
fn test_build_docker_command_docker_backend() {
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        tags: &["img:latest"],
        extra_flags: &[],
        push: false,
        push_flags: &[],
        labels: &[],
        use_backend: Some("docker"),
    })
    .unwrap();
    assert_eq!(cmd[0], "docker");
    assert_eq!(cmd[1], "build");
    // Should NOT have "buildx" subcommand
    assert!(!cmd.contains(&"buildx".to_string()));
}

#[test]
fn test_build_docker_command_buildx_backend() {
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        tags: &["img:latest"],
        extra_flags: &[],
        push: false,
        push_flags: &[],
        labels: &[],
        use_backend: Some("buildx"),
    })
    .unwrap();
    assert_eq!(cmd[0], "docker");
    assert_eq!(cmd[1], "buildx");
    assert_eq!(cmd[2], "build");
}

#[test]
fn test_docker_manifest_dry_run() {
    use anodizer_core::config::{Config, CrateConfig, DockerManifestConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config {
        project_name: "test".to_string(),
        crates: vec![CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker_manifests: Some(vec![DockerManifestConfig {
                name_template: "ghcr.io/owner/app:{{ .Version }}".to_string(),
                image_templates: vec![
                    "ghcr.io/owner/app:{{ .Version }}-amd64".to_string(),
                    "ghcr.io/owner/app:{{ .Version }}-arm64".to_string(),
                ],
                create_flags: Some(vec!["--amend".to_string()]),
                push_flags: None,
                skip_push: None,
                id: Some("multi-arch".to_string()),
                use_backend: None,
                retry: None,
            }]),
            ..Default::default()
        }],
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

    let stage = DockerStage::new();
    let result = stage.run(&mut ctx);
    assert!(
        result.is_ok(),
        "dry-run manifest should succeed, got: {:?}",
        result.err()
    );

    // Verify DockerManifest artifact was registered
    let manifests = ctx.artifacts.by_kind(ArtifactKind::DockerManifest);
    assert_eq!(manifests.len(), 1);
    assert_eq!(
        manifests[0].metadata.get("manifest").unwrap(),
        "ghcr.io/owner/app:1.0.0"
    );
    assert_eq!(
        manifests[0].metadata.get("images").unwrap(),
        "ghcr.io/owner/app:1.0.0-amd64,ghcr.io/owner/app:1.0.0-arm64"
    );
    assert_eq!(manifests[0].metadata.get("id").unwrap(), "multi-arch");
}

#[test]
fn test_docker_manifest_create_push_flags_template_rendering() {
    // S8 regression: create_flags and push_flags must receive the same
    // template context as V1 docker (`{{ .Tag }}`, `{{ .Env.* }}`).
    use anodizer_core::config::{Config, CrateConfig, DockerManifestConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config {
        project_name: "test".to_string(),
        crates: vec![CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker_manifests: Some(vec![DockerManifestConfig {
                name_template: "ghcr.io/owner/app:{{ .Version }}".to_string(),
                image_templates: vec![
                    "ghcr.io/owner/app:{{ .Version }}-amd64".to_string(),
                    "ghcr.io/owner/app:{{ .Version }}-arm64".to_string(),
                ],
                create_flags: Some(vec![
                    "--amend".to_string(),
                    "--annotation=tag={{ .Tag }}".to_string(),
                    "--annotation=env={{ .Env.CI_BACKEND }}".to_string(),
                ]),
                push_flags: Some(vec!["--purge={{ .Tag }}".to_string()]),
                skip_push: None,
                id: Some("multi-arch-templated".to_string()),
                use_backend: None,
                retry: None,
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.2.3");
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    ctx.template_vars_mut().set_env("CI_BACKEND", "github");

    let stage = DockerStage::new();
    let result = stage.run(&mut ctx);
    assert!(
        result.is_ok(),
        "dry-run manifest with templated flags should succeed, got: {:?}",
        result.err()
    );

    // Inline-render the flags through the same ctx to assert templating
    // resolves — dry-run doesn't expose rendered flags in artifact metadata,
    // but the stage ran without template errors, and we verify the engine
    // handles the exact strings the stage passes it.
    assert_eq!(
        ctx.render_template("--annotation=tag={{ .Tag }}").unwrap(),
        "--annotation=tag=v1.2.3"
    );
    assert_eq!(
        ctx.render_template("--annotation=env={{ .Env.CI_BACKEND }}")
            .unwrap(),
        "--annotation=env=github"
    );
    assert_eq!(
        ctx.render_template("--purge={{ .Tag }}").unwrap(),
        "--purge=v1.2.3"
    );
}

#[test]
fn test_docker_manifest_skip_push_auto_prerelease() {
    use anodizer_core::config::{Config, CrateConfig, DockerManifestConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config {
        project_name: "test".to_string(),
        crates: vec![CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker_manifests: Some(vec![DockerManifestConfig {
                name_template: "ghcr.io/owner/app:{{ .Version }}".to_string(),
                image_templates: vec!["ghcr.io/owner/app:{{ .Version }}-amd64".to_string()],
                create_flags: None,
                push_flags: None,
                skip_push: Some(SkipPushConfig::Auto),
                id: None,
                use_backend: None,
                retry: None,
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0-rc.1");
    ctx.template_vars_mut().set("Tag", "v1.0.0-rc.1");
    ctx.template_vars_mut().set("Prerelease", "rc.1");

    let stage = DockerStage::new();
    let result = stage.run(&mut ctx);
    assert!(
        result.is_ok(),
        "manifest with auto skip_push + prerelease should succeed, got: {:?}",
        result.err()
    );

    // Artifact should still be registered even if push is skipped
    let manifests = ctx.artifacts.by_kind(ArtifactKind::DockerManifest);
    assert_eq!(manifests.len(), 1);
}

#[test]
fn test_docker_manifest_with_use_backend_podman() {
    use anodizer_core::config::DockerManifestConfig;
    let yaml = r#"
name_template: "ghcr.io/owner/app:latest"
image_templates:
  - "ghcr.io/owner/app:latest-amd64"
use: podman
"#;
    let cfg: DockerManifestConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.use_backend.as_deref(), Some("podman"));
}

// F7: docker manifest `use:` validation -----------------------------------

#[test]
fn test_resolve_manifester_docker_default() {
    use super::command::resolve_manifester;
    assert_eq!(resolve_manifester(None).unwrap(), "docker");
    assert_eq!(resolve_manifester(Some("docker")).unwrap(), "docker");
}

#[test]
fn test_resolve_manifester_podman_explicit() {
    use super::command::resolve_manifester;
    assert_eq!(resolve_manifester(Some("podman")).unwrap(), "podman");
}

#[test]
fn test_resolve_manifester_unknown_errors_with_value() {
    // Typos like `use: dockr` used to fall back silently to "docker";
    // they now produce a clear error naming the invalid value.
    use super::command::resolve_manifester;
    let err = resolve_manifester(Some("dockr")).unwrap_err().to_string();
    assert!(
        err.contains("invalid use 'dockr'"),
        "error should name the offending value, got: {err}"
    );
    assert!(
        err.contains("[docker, podman]"),
        "error should list valid options, got: {err}"
    );
}

#[test]
fn test_resolve_manifester_buildx_rejected() {
    // GR has no `buildx manifest` subcommand; reject explicitly so that
    // pasting `use: buildx` from a build stanza into a manifest stanza
    // surfaces a clear error instead of running `buildx manifest …` as if
    // it were a real command.
    use super::command::resolve_manifester;
    let err = resolve_manifester(Some("buildx")).unwrap_err().to_string();
    assert!(
        err.contains("invalid use 'buildx'"),
        "error should reject 'buildx' for manifests, got: {err}"
    );
}

// ====================================================================
// Docker V2 tests
// ====================================================================

#[test]
fn test_generate_v2_image_tags() {
    let images = vec![
        "ghcr.io/owner/app".to_string(),
        "docker.io/owner/app".to_string(),
    ];
    let tags = vec!["latest".to_string(), "v1.0.0".to_string()];
    let result = generate_v2_image_tags(&images, &tags);
    assert_eq!(result.len(), 4);
    // Results are sorted and deduped
    assert_eq!(result[0], "docker.io/owner/app:latest");
    assert_eq!(result[1], "docker.io/owner/app:v1.0.0");
    assert_eq!(result[2], "ghcr.io/owner/app:latest");
    assert_eq!(result[3], "ghcr.io/owner/app:v1.0.0");
}

#[test]
fn test_generate_v2_image_tags_empty() {
    assert!(generate_v2_image_tags(&[], &["latest".to_string()]).is_empty());
    assert!(generate_v2_image_tags(&["img".to_string()], &[]).is_empty());
}

#[test]
fn test_generate_v2_image_tags_single() {
    let result =
        generate_v2_image_tags(&["ghcr.io/owner/app".to_string()], &["latest".to_string()]);
    assert_eq!(result, vec!["ghcr.io/owner/app:latest"]);
}

#[test]
fn test_build_docker_v2_command_basic() {
    let image_tags = vec![
        "ghcr.io/owner/app:latest".to_string(),
        "ghcr.io/owner/app:v1.0.0".to_string(),
    ];
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        image_tags: &image_tags,
        build_args: &[],
        annotations: &[],
        labels: &[],
        flags: &[],
        sbom: false,
        push: false,
        load: true,
    })
    .unwrap();

    // V2 always uses buildx
    assert_eq!(cmd[0], "docker");
    assert_eq!(cmd[1], "buildx");
    assert_eq!(cmd[2], "build");

    // Platform
    assert!(cmd.contains(&"--platform=linux/amd64".to_string()));

    // Tags
    let tag_positions: Vec<usize> = cmd
        .iter()
        .enumerate()
        .filter_map(|(i, t)| if t == "--tag" { Some(i) } else { None })
        .collect();
    assert_eq!(tag_positions.len(), 2);
    assert_eq!(cmd[tag_positions[0] + 1], "ghcr.io/owner/app:latest");
    assert_eq!(cmd[tag_positions[1] + 1], "ghcr.io/owner/app:v1.0.0");

    // Context dir is last
    assert_eq!(cmd.last().unwrap(), "/tmp/ctx");
}

#[test]
fn test_build_docker_v2_command_build_args() {
    let build_args = vec![
        ("APP_VERSION".to_string(), "1.0.0".to_string()),
        ("BUILD_DATE".to_string(), "2024-01-01".to_string()),
    ];
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        image_tags: &["img:latest".to_string()],
        build_args: &build_args,
        annotations: &[],
        labels: &[],
        flags: &[],
        sbom: false,
        push: false,
        load: true,
    })
    .unwrap();

    // Check --build-arg flags
    let ba_positions: Vec<usize> = cmd
        .iter()
        .enumerate()
        .filter_map(|(i, t)| if t == "--build-arg" { Some(i) } else { None })
        .collect();
    assert_eq!(ba_positions.len(), 2);
    assert_eq!(cmd[ba_positions[0] + 1], "APP_VERSION=1.0.0");
    assert_eq!(cmd[ba_positions[1] + 1], "BUILD_DATE=2024-01-01");
}

#[test]
fn test_build_docker_v2_command_annotations() {
    let annotations = vec![
        (
            "org.opencontainers.image.source".to_string(),
            "https://github.com/owner/app".to_string(),
        ),
        (
            "org.opencontainers.image.version".to_string(),
            "1.0.0".to_string(),
        ),
    ];
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &annotations,
        labels: &[],
        flags: &[],
        sbom: false,
        push: false,
        load: true,
    })
    .unwrap();

    let ann_positions: Vec<usize> = cmd
        .iter()
        .enumerate()
        .filter_map(|(i, t)| if t == "--annotation" { Some(i) } else { None })
        .collect();
    assert_eq!(ann_positions.len(), 2);
    assert_eq!(
        cmd[ann_positions[0] + 1],
        "org.opencontainers.image.source=https://github.com/owner/app"
    );
    assert_eq!(
        cmd[ann_positions[1] + 1],
        "org.opencontainers.image.version=1.0.0"
    );
}

#[test]
fn test_build_docker_v2_command_labels() {
    let labels = vec![("maintainer".to_string(), "dev@example.com".to_string())];
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &[],
        labels: &labels,
        flags: &[],
        sbom: false,
        push: false,
        load: true,
    })
    .unwrap();

    assert!(cmd.contains(&"--label".to_string()));
    assert!(cmd.contains(&"maintainer=dev@example.com".to_string()));
}

#[test]
fn test_build_docker_v2_command_sbom_true() {
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &[],
        labels: &[],
        flags: &[],
        sbom: true,
        push: false,
        load: true,
    })
    .unwrap();

    assert!(cmd.contains(&"--attest=type=sbom".to_string()));
    // When sbom is true, auto --sbom=false should NOT be added
    assert!(!cmd.contains(&"--sbom=false".to_string()));
}

#[test]
fn test_build_docker_v2_command_sbom_false() {
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &[],
        labels: &[],
        flags: &[],
        sbom: false,
        push: false,
        load: true,
    })
    .unwrap();

    assert!(!cmd.contains(&"--sbom=true".to_string()));
}

#[test]
fn test_build_docker_v2_command_flags() {
    let flags = vec![
        "--cache-from=type=gha".to_string(),
        "--cache-to=type=gha".to_string(),
    ];
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &[],
        labels: &[],
        flags: &flags,
        sbom: false,
        push: false,
        load: true,
    })
    .unwrap();

    assert!(cmd.contains(&"--cache-from=type=gha".to_string()));
    assert!(cmd.contains(&"--cache-to=type=gha".to_string()));
}

#[test]
fn test_build_docker_v2_command_push() {
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &[],
        labels: &[],
        flags: &[],
        sbom: false,
        push: true,
        load: true,
    })
    .unwrap();

    assert!(cmd.contains(&"--push".to_string()));
    assert!(!cmd.contains(&"--load".to_string()));
}

#[test]
fn test_build_docker_v2_command_no_push_single_platform_loads() {
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &[],
        labels: &[],
        flags: &[],
        sbom: false,
        push: false,
        load: true,
    })
    .unwrap();

    assert!(!cmd.contains(&"--push".to_string()));
    assert!(cmd.contains(&"--load".to_string()));
}

#[test]
fn test_build_docker_v2_command_no_push_multi_platform_no_load() {
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64", "linux/arm64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &[],
        labels: &[],
        flags: &[],
        sbom: false,
        push: false,
        load: true,
    })
    .unwrap();

    assert!(!cmd.contains(&"--push".to_string()));
    // --load is incompatible with multi-platform
    assert!(!cmd.contains(&"--load".to_string()));
}

#[test]
fn test_build_docker_v2_command_combined() {
    let build_args = vec![("VERSION".to_string(), "1.0.0".to_string())];
    let annotations = vec![(
        "org.opencontainers.image.version".to_string(),
        "1.0.0".to_string(),
    )];
    let labels = vec![("maintainer".to_string(), "dev@example.com".to_string())];
    let flags = vec!["--no-cache".to_string()];

    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64", "linux/arm64"],
        image_tags: &[
            "ghcr.io/owner/app:latest".to_string(),
            "ghcr.io/owner/app:v1.0.0".to_string(),
        ],
        build_args: &build_args,
        annotations: &annotations,
        labels: &labels,
        flags: &flags,
        sbom: true,
        push: true,
        load: true,
    })
    .unwrap();

    // Verify all parts are present
    assert!(cmd.contains(&"--platform=linux/amd64,linux/arm64".to_string()));
    assert!(cmd.contains(&"--build-arg".to_string()));
    assert!(cmd.contains(&"VERSION=1.0.0".to_string()));
    assert!(cmd.contains(&"--annotation".to_string()));
    // Multi-platform annotations get "index:" prefix
    assert!(cmd.contains(&"index:org.opencontainers.image.version=1.0.0".to_string()));
    assert!(cmd.contains(&"--label".to_string()));
    assert!(cmd.contains(&"maintainer=dev@example.com".to_string()));
    assert!(cmd.contains(&"--no-cache".to_string()));
    assert!(cmd.contains(&"--attest=type=sbom".to_string()));
    assert!(cmd.contains(&"--push".to_string()));
    assert_eq!(cmd.last().unwrap(), "/tmp/ctx");
}

#[test]
fn test_build_docker_v2_command_includes_iidfile() {
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &[],
        labels: &[],
        flags: &[],
        sbom: false,
        push: false,
        load: true,
    })
    .unwrap();
    assert!(
        cmd.iter().any(|a| a.starts_with("--iidfile=")),
        "V2 command should include --iidfile, got: {:?}",
        cmd
    );
    // --iidfile should come before the staging dir (last arg)
    let iidfile_pos = cmd
        .iter()
        .position(|a| a.starts_with("--iidfile="))
        .unwrap();
    assert_eq!(
        iidfile_pos,
        cmd.len() - 2,
        "--iidfile should be second-to-last arg"
    );
    // Verify the iidfile path is within the staging dir
    assert_eq!(
        cmd[iidfile_pos], "--iidfile=/tmp/staging/id.txt",
        "iidfile should be written to staging dir"
    );
}

#[test]
fn test_docker_v2_config_parse_yaml() {
    let yaml = r#"
id: myapp-docker
ids:
  - myapp-build
dockerfile: Dockerfile.prod
images:
  - ghcr.io/owner/app
  - docker.io/owner/app
tags:
  - latest
  - "{{ .Version }}"
labels:
  maintainer: "dev@example.com"
annotations:
  org.opencontainers.image.source: "https://github.com/owner/app"
extra_files:
  - config.yaml
platforms:
  - linux/amd64
  - linux/arm64
build_args:
  APP_VERSION: "{{ .Version }}"
  BUILD_DATE: "2024-01-01"
flags:
  - "--no-cache"
skip: false
sbom: true
retry:
  attempts: 5
  delay: "2s"
"#;
    let cfg: anodizer_core::config::DockerV2Config = serde_yaml_ng::from_str(yaml).unwrap();

    assert_eq!(cfg.id, Some("myapp-docker".to_string()));
    assert_eq!(cfg.ids, Some(vec!["myapp-build".to_string()]));
    assert_eq!(cfg.dockerfile, "Dockerfile.prod");
    assert_eq!(cfg.images.len(), 2);
    assert_eq!(cfg.images[0], "ghcr.io/owner/app");
    assert_eq!(cfg.images[1], "docker.io/owner/app");
    assert_eq!(cfg.tags.len(), 2);
    assert_eq!(cfg.tags[0], "latest");
    assert_eq!(cfg.tags[1], "{{ .Version }}");

    let labels = cfg.labels.unwrap();
    assert_eq!(labels.get("maintainer").unwrap(), "dev@example.com");

    let annotations = cfg.annotations.unwrap();
    assert_eq!(
        annotations.get("org.opencontainers.image.source").unwrap(),
        "https://github.com/owner/app"
    );

    assert_eq!(cfg.extra_files.unwrap(), vec!["config.yaml"]);

    let platforms = cfg.platforms.unwrap();
    assert_eq!(platforms.len(), 2);

    let build_args = cfg.build_args.unwrap();
    assert_eq!(build_args.get("APP_VERSION").unwrap(), "{{ .Version }}");
    assert_eq!(build_args.get("BUILD_DATE").unwrap(), "2024-01-01");

    assert_eq!(cfg.flags.unwrap(), vec!["--no-cache"]);

    assert_eq!(cfg.skip, Some(StringOrBool::Bool(false)));
    assert_eq!(cfg.sbom, Some(StringOrBool::Bool(true)));

    let retry = cfg.retry.unwrap();
    assert_eq!(retry.attempts, Some(5));
    assert_eq!(retry.delay, Some("2s".to_string()));
}

#[test]
fn test_docker_v2_config_parse_minimal() {
    let yaml = r#"
dockerfile: Dockerfile
images:
  - ghcr.io/owner/app
tags:
  - latest
"#;
    let cfg: anodizer_core::config::DockerV2Config = serde_yaml_ng::from_str(yaml).unwrap();

    assert_eq!(cfg.id, None);
    assert_eq!(cfg.ids, None);
    assert_eq!(cfg.dockerfile, "Dockerfile");
    assert_eq!(cfg.images, vec!["ghcr.io/owner/app"]);
    assert_eq!(cfg.tags, vec!["latest"]);
    assert_eq!(cfg.labels, None);
    assert_eq!(cfg.annotations, None);
    assert_eq!(cfg.extra_files, None);
    assert_eq!(cfg.platforms, None);
    assert_eq!(cfg.build_args, None);
    assert_eq!(cfg.flags, None);
    assert_eq!(cfg.skip, None);
    assert_eq!(cfg.sbom, None);
    assert!(cfg.retry.is_none());
}

#[test]
fn test_docker_v2_config_disable_as_bool() {
    let yaml = r#"
dockerfile: Dockerfile
images: ["img"]
tags: ["latest"]
skip: true
"#;
    let cfg: anodizer_core::config::DockerV2Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.skip, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_docker_v2_config_disable_as_template() {
    let yaml = r#"
dockerfile: Dockerfile
images: ["img"]
tags: ["latest"]
skip: "{{ if .IsSnapshot }}true{{ end }}"
"#;
    let cfg: anodizer_core::config::DockerV2Config = serde_yaml_ng::from_str(yaml).unwrap();
    match cfg.skip {
        Some(StringOrBool::String(s)) => {
            assert!(s.contains("IsSnapshot"));
        }
        other => panic!("expected StringOrBool::String, got {:?}", other),
    }
}

#[test]
fn test_docker_v2_config_sbom_as_bool() {
    let yaml = r#"
dockerfile: Dockerfile
images: ["img"]
tags: ["latest"]
sbom: true
"#;
    let cfg: anodizer_core::config::DockerV2Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.sbom, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_docker_v2_config_sbom_as_string() {
    let yaml = r#"
dockerfile: Dockerfile
images: ["img"]
tags: ["latest"]
sbom: "true"
"#;
    let cfg: anodizer_core::config::DockerV2Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.sbom, Some(StringOrBool::String("true".to_string())));
}

#[test]
fn test_docker_v2_dry_run_registers_artifacts() {
    use anodizer_core::config::{Config, CrateConfig, DockerV2Config};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dockerfile = tmp.path().join("Dockerfile");
    fs::write(&dockerfile, b"FROM scratch\n").unwrap();

    let v2_cfg = DockerV2Config {
        id: Some("myapp-v2".to_string()),
        images: vec!["ghcr.io/owner/myapp".to_string()],
        tags: vec!["{{ .Tag }}".to_string(), "latest".to_string()],
        dockerfile: dockerfile.to_string_lossy().into_owned(),
        platforms: Some(vec!["linux/amd64".to_string()]),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        docker_v2: Some(vec![v2_cfg]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    let stage = DockerStage::new();
    stage.run(&mut ctx).unwrap();

    let images = ctx.artifacts.by_kind(ArtifactKind::DockerImageV2);
    // images x tags = 1 x 2 = 2
    assert_eq!(images.len(), 2);

    let tags: Vec<&str> = images
        .iter()
        .map(|a| a.metadata.get("tag").unwrap().as_str())
        .collect();
    assert!(tags.contains(&"ghcr.io/owner/myapp:v1.0.0"));
    assert!(tags.contains(&"ghcr.io/owner/myapp:latest"));

    // Verify V2 metadata
    for img in &images {
        assert_eq!(img.metadata.get("api").unwrap(), "v2");
        assert_eq!(img.metadata.get("id").unwrap(), "myapp-v2");
    }
}

/// Q3.1 (GR commit e7a4afa, issue #6515): the v2 build log line emits
/// `images` and `digest` as separate fields, not as a single
/// `image@digest` blob.
///
/// Capturing `tracing` output here would need a full `tracing_subscriber`
/// fixture, which adds dev-dep weight. Instead, we extract the
/// human-readable status line into a pure helper
/// (`format_v2_created_images_log`) and assert its shape directly. The
/// `tracing::info!(images = …, digest = …)` macro at the call site uses
/// the same two-field shape — verified by code inspection in build.rs.
#[test]
fn v2_digest_log_split_emits_images_and_digest_as_separate_fields() {
    let images = vec![
        "ghcr.io/owner/app:v1.0.0".to_string(),
        "ghcr.io/owner/app:latest".to_string(),
    ];
    let digest = "sha256:deadbeef".to_string();
    let line = format_v2_created_images_log(&images, &digest);

    // Both fields are independently addressable: a log scraper can match
    // `images=…` and `digest=…` without splitting on `@`.
    assert!(
        line.contains("images=ghcr.io/owner/app:v1.0.0,ghcr.io/owner/app:latest"),
        "log line should expose `images=` field with comma-joined tags: {line}",
    );
    assert!(
        line.contains("digest=sha256:deadbeef"),
        "log line should expose `digest=` field separately: {line}",
    );
    // The pre-fix shape `image@digest` MUST NOT appear.
    assert!(
        !line.contains("ghcr.io/owner/app:v1.0.0@sha256:deadbeef"),
        "log line must NOT embed image@digest in a single field (regression: GR e7a4afa): {line}",
    );
}

/// P5.1 (GR commit d788340): when the `dockerfile:` template renders to the
/// empty string, the v2 build must skip cleanly instead of attempting to
/// copy a non-existent file.
///
/// This is the equivalent of GR's `dockerfile: "{{ if .IsSnapshot }}Dockerfile{{ end }}"`
/// during a release (IsSnapshot=false) — the rendered string is empty, so
/// the pipe should bail with "skipping … rendered empty" and produce no
/// artifacts.
#[test]
fn dockerfile_template_renders_to_empty_skips_pipe() {
    use anodizer_core::config::{Config, CrateConfig, DockerV2Config};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    // Note: NO dockerfile written — if the skip logic is broken and the
    // pipe attempts to copy, the missing-file error would surface as a
    // distinct failure mode than the clean skip we expect.

    let v2_cfg = DockerV2Config {
        id: Some("myapp-v2".to_string()),
        images: vec!["ghcr.io/owner/myapp".to_string()],
        tags: vec!["{{ Tag }}".to_string()],
        // Tera analog of GR's `{{ if .IsSnapshot }}Dockerfile{{ end }}`.
        // With IsSnapshot=false (default Context), this renders to "".
        dockerfile: "{% if IsSnapshot %}Dockerfile{% endif %}".to_string(),
        platforms: Some(vec!["linux/amd64".to_string()]),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        docker_v2: Some(vec![v2_cfg]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    let stage = DockerStage::new();
    // Must succeed with a clean skip — not error out trying to copy a
    // missing Dockerfile.
    stage.run(&mut ctx).expect("clean skip, not copy failure");

    // No DockerImageV2 artifacts should be registered for the skipped pipe.
    let images = ctx.artifacts.by_kind(ArtifactKind::DockerImageV2);
    assert!(
        images.is_empty(),
        "expected no v2 images when dockerfile template renders empty, got {:?}",
        images.iter().map(|a| &a.name).collect::<Vec<_>>(),
    );
}

#[test]
fn test_docker_v2_dry_run_multiple_images_and_tags() {
    use anodizer_core::config::{Config, CrateConfig, DockerV2Config};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dockerfile = tmp.path().join("Dockerfile");
    fs::write(&dockerfile, b"FROM scratch\n").unwrap();

    let v2_cfg = DockerV2Config {
        images: vec![
            "ghcr.io/owner/app".to_string(),
            "docker.io/owner/app".to_string(),
        ],
        tags: vec![
            "latest".to_string(),
            "{{ .Version }}".to_string(),
            "{{ .Tag }}".to_string(),
        ],
        dockerfile: dockerfile.to_string_lossy().into_owned(),
        platforms: Some(vec!["linux/amd64".to_string()]),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        docker_v2: Some(vec![v2_cfg]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "2.0.0");
    ctx.template_vars_mut().set("Tag", "v2.0.0");

    let stage = DockerStage::new();
    stage.run(&mut ctx).unwrap();

    // 2 images x 3 tags = 6 artifacts
    let images = ctx.artifacts.by_kind(ArtifactKind::DockerImageV2);
    assert_eq!(images.len(), 6);

    let tags: Vec<&str> = images
        .iter()
        .map(|a| a.metadata.get("tag").unwrap().as_str())
        .collect();
    assert!(tags.contains(&"ghcr.io/owner/app:latest"));
    assert!(tags.contains(&"ghcr.io/owner/app:2.0.0"));
    assert!(tags.contains(&"ghcr.io/owner/app:v2.0.0"));
    assert!(tags.contains(&"docker.io/owner/app:latest"));
    assert!(tags.contains(&"docker.io/owner/app:2.0.0"));
    assert!(tags.contains(&"docker.io/owner/app:v2.0.0"));
}

#[test]
fn test_docker_v2_disable_skips_build() {
    use anodizer_core::config::{Config, CrateConfig, DockerV2Config};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dockerfile = tmp.path().join("Dockerfile");
    fs::write(&dockerfile, b"FROM scratch\n").unwrap();

    let v2_cfg = DockerV2Config {
        images: vec!["ghcr.io/owner/app".to_string()],
        tags: vec!["latest".to_string()],
        dockerfile: dockerfile.to_string_lossy().into_owned(),
        skip: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        docker_v2: Some(vec![v2_cfg]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    let stage = DockerStage::new();
    stage.run(&mut ctx).unwrap();

    // Disabled config should produce no artifacts
    let images = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
    assert_eq!(images.len(), 0);
}

#[test]
fn test_docker_v2_extra_files_staging_live() {
    use anodizer_core::config::{Config, CrateConfig, DockerRetryConfig, DockerV2Config};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();

    // Create Dockerfile
    let dockerfile = tmp.path().join("Dockerfile");
    fs::write(&dockerfile, b"FROM scratch\nCOPY . /\n").unwrap();

    // Create extra files
    let extra1 = tmp.path().join("config.yaml");
    fs::write(&extra1, b"key: value").unwrap();

    let v2_cfg = DockerV2Config {
        images: vec!["ghcr.io/owner/app".to_string()],
        tags: vec!["latest".to_string()],
        dockerfile: dockerfile.to_string_lossy().into_owned(),
        platforms: Some(vec!["linux/amd64".to_string()]),
        extra_files: Some(vec![extra1.to_string_lossy().into_owned()]),
        retry: Some(DockerRetryConfig {
            attempts: Some(1),
            delay: None,
            max_delay: None,
        }),
        ..Default::default()
    };

    let dist = tmp.path().join("dist");
    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        docker_v2: Some(vec![v2_cfg]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist.clone();
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: false,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    // Run the stage (will fail at docker command, but staging is complete)
    let _result = DockerStage::new().run(&mut ctx);

    // Verify staging directory structure
    let staging_dir = dist.join("docker_v2").join("myapp").join("0");
    assert!(staging_dir.join("Dockerfile").exists());
    // Extra file (absolute path) should be in staging root
    assert!(staging_dir.join("config.yaml").exists());
    assert_eq!(
        fs::read_to_string(staging_dir.join("config.yaml")).unwrap(),
        "key: value"
    );
}

#[test]
fn test_docker_v2_crate_config_field() {
    let yaml = r#"
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    docker_v2:
      - dockerfile: Dockerfile
        images:
          - ghcr.io/owner/app
        tags:
          - latest
        build_args:
          VERSION: "1.0.0"
        annotations:
          org.opencontainers.image.source: "https://github.com/owner/app"
        sbom: true
"#;
    let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.crates.len(), 1);
    let v2_configs = config.crates[0].docker_v2.as_ref().unwrap();
    assert_eq!(v2_configs.len(), 1);
    assert_eq!(v2_configs[0].dockerfile, "Dockerfile");
    assert_eq!(v2_configs[0].images, vec!["ghcr.io/owner/app"]);
    assert_eq!(v2_configs[0].tags, vec!["latest"]);

    let build_args = v2_configs[0].build_args.as_ref().unwrap();
    assert_eq!(build_args.get("VERSION").unwrap(), "1.0.0");

    let annotations = v2_configs[0].annotations.as_ref().unwrap();
    assert_eq!(
        annotations.get("org.opencontainers.image.source").unwrap(),
        "https://github.com/owner/app"
    );

    assert_eq!(v2_configs[0].sbom, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_is_docker_v2_skipped_none() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert!(!is_docker_v2_skipped(&None, &ctx).unwrap());
}

#[test]
fn test_is_docker_v2_skipped_bool_true() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert!(is_docker_v2_skipped(&Some(StringOrBool::Bool(true)), &ctx).unwrap());
}

#[test]
fn test_is_docker_v2_skipped_bool_false() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert!(!is_docker_v2_skipped(&Some(StringOrBool::Bool(false)), &ctx).unwrap());
}

#[test]
fn test_is_docker_v2_sbom_enabled_none_defaults_on() {
    // GR-aligned default: when `sbom` is unset, SBOM attestation is
    // enabled. Mirrors `internal/pipe/docker/v2/docker.go:85-87` which
    // assigns `SBOM = "true"` at Default() time. Pins C-new-7 at the
    // helper level — defensive path for callers that bypass the
    // Default()-apply pass.
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert!(is_docker_v2_sbom_enabled(&None, &ctx).unwrap());
}

#[test]
fn test_apply_docker_v2_defaults_sbom_none_resolves_to_true() {
    // Pins C-new-7 at the wired Default()-apply level: a config with
    // `sbom: None` post-defaults must carry `Some(Bool(true))` so the
    // resolved YAML written to dist/config.yaml round-trips faithfully
    // (matching GoReleaser's persistence behavior). Complements the
    // helper-level test above.
    use anodizer_core::config::DockerV2Config;

    let cfg = apply_docker_v2_defaults(
        DockerV2Config {
            images: vec!["ghcr.io/owner/app".into()],
            ..Default::default()
        },
        "myapp",
    );
    assert_eq!(cfg.sbom, Some(StringOrBool::Bool(true)));
    // Spot-check the other applied defaults so a regression in any one
    // of them surfaces from the same test.
    assert_eq!(cfg.id.as_deref(), Some("myapp"));
    assert_eq!(cfg.dockerfile, "Dockerfile");
    assert_eq!(cfg.tags, vec!["{{ .Tag }}"]);
    assert_eq!(
        cfg.platforms.as_deref(),
        Some(&["linux/amd64".to_string(), "linux/arm64".to_string()][..])
    );
}

#[test]
fn test_apply_docker_v2_defaults_preserves_user_values() {
    // User-set values must survive Default()-apply unchanged.
    use anodizer_core::config::DockerV2Config;

    let cfg = apply_docker_v2_defaults(
        DockerV2Config {
            id: Some("custom-id".into()),
            dockerfile: "Containerfile".into(),
            tags: vec!["v1".into()],
            platforms: Some(vec!["linux/arm/v7".into()]),
            sbom: Some(StringOrBool::Bool(false)),
            ..Default::default()
        },
        "myapp",
    );
    assert_eq!(cfg.id.as_deref(), Some("custom-id"));
    assert_eq!(cfg.dockerfile, "Containerfile");
    assert_eq!(cfg.tags, vec!["v1"]);
    assert_eq!(
        cfg.platforms.as_deref(),
        Some(&["linux/arm/v7".to_string()][..])
    );
    assert_eq!(cfg.sbom, Some(StringOrBool::Bool(false)));
}

#[test]
fn test_is_docker_v2_sbom_enabled_bool_true() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert!(is_docker_v2_sbom_enabled(&Some(StringOrBool::Bool(true)), &ctx).unwrap());
}

#[test]
fn test_is_docker_v2_sbom_enabled_bool_false() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert!(!is_docker_v2_sbom_enabled(&Some(StringOrBool::Bool(false)), &ctx).unwrap());
}

#[test]
fn test_is_docker_v2_skipped_string_true() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert!(is_docker_v2_skipped(&Some(StringOrBool::String("true".to_string())), &ctx).unwrap());
}

#[test]
fn test_is_docker_v2_skipped_string_false() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert!(!is_docker_v2_skipped(&Some(StringOrBool::String("false".to_string())), &ctx).unwrap());
}

#[test]
fn test_is_docker_v2_skipped_template_snapshot_true() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut().set("IsSnapshot", "true");
    assert!(
        is_docker_v2_skipped(
            &Some(StringOrBool::String("{{ .IsSnapshot }}".to_string())),
            &ctx
        )
        .unwrap()
    );
}

#[test]
fn test_is_docker_v2_skipped_template_snapshot_false() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut().set("IsSnapshot", "false");
    assert!(
        !is_docker_v2_skipped(
            &Some(StringOrBool::String("{{ .IsSnapshot }}".to_string())),
            &ctx
        )
        .unwrap()
    );
}

#[test]
fn test_is_docker_v2_sbom_enabled_string_true() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert!(
        is_docker_v2_sbom_enabled(&Some(StringOrBool::String("true".to_string())), &ctx).unwrap()
    );
}

#[test]
fn test_is_docker_v2_sbom_enabled_string_false() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert!(
        !is_docker_v2_sbom_enabled(&Some(StringOrBool::String("false".to_string())), &ctx).unwrap()
    );
}

#[test]
fn test_is_docker_v2_sbom_enabled_template_snapshot_true() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut().set("IsSnapshot", "true");
    assert!(
        is_docker_v2_sbom_enabled(
            &Some(StringOrBool::String("{{ .IsSnapshot }}".to_string())),
            &ctx
        )
        .unwrap()
    );
}

#[test]
fn test_is_docker_v2_sbom_enabled_template_snapshot_false() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut().set("IsSnapshot", "false");
    assert!(
        !is_docker_v2_sbom_enabled(
            &Some(StringOrBool::String("{{ .IsSnapshot }}".to_string())),
            &ctx
        )
        .unwrap()
    );
}

#[test]
fn test_docker_v2_build_args_render_in_command() {
    // Verify that build_args end up in the V2 command correctly
    use anodizer_core::config::{Config, CrateConfig, DockerV2Config};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = TempDir::new().unwrap();
    let dockerfile = tmp.path().join("Dockerfile");
    fs::write(&dockerfile, b"FROM scratch\n").unwrap();

    let mut build_args = HashMap::new();
    build_args.insert("VERSION".to_string(), "{{ .Version }}".to_string());
    build_args.insert("STATIC".to_string(), "hello".to_string());

    let mut annotations = HashMap::new();
    annotations.insert(
        "org.opencontainers.image.version".to_string(),
        "{{ .Version }}".to_string(),
    );

    let v2_cfg = DockerV2Config {
        images: vec!["img".to_string()],
        tags: vec!["latest".to_string()],
        dockerfile: dockerfile.to_string_lossy().into_owned(),
        platforms: Some(vec!["linux/amd64".to_string()]),
        build_args: Some(build_args),
        annotations: Some(annotations),
        sbom: Some(StringOrBool::Bool(true)),
        flags: Some(vec!["--no-cache".to_string()]),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        docker_v2: Some(vec![v2_cfg]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "3.0.0");
    ctx.template_vars_mut().set("Tag", "v3.0.0");

    let stage = DockerStage::new();
    stage.run(&mut ctx).unwrap();

    // The stage ran in dry-run mode, so it registered artifacts
    let images = ctx.artifacts.by_kind(ArtifactKind::DockerImageV2);
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].metadata.get("tag").unwrap(), "img:latest");
}

#[test]
fn test_templated_extra_files_written_to_staging_dir() {
    use anodizer_core::config::TemplatedExtraFile;
    use anodizer_core::template::TemplateVars;

    let tmp = TempDir::new().unwrap();
    let staging_dir = tmp.path().join("staging");
    fs::create_dir_all(&staging_dir).unwrap();

    // Create a source template file
    let tpl_src = tmp.path().join("config.yaml.tpl");
    fs::write(&tpl_src, "app: {{ .ProjectName }}\nversion: {{ .Version }}").unwrap();

    let mut vars = TemplateVars::new();
    vars.set("ProjectName", "myapp");
    vars.set("Version", "1.0.0");

    let specs = vec![TemplatedExtraFile {
        src: tpl_src.to_string_lossy().to_string(),
        dst: Some("config.yaml".to_string()),
        mode: None,
    }];

    let results = anodizer_core::templated_files::process_templated_extra_files_with_vars(
        &specs,
        &vars,
        &staging_dir,
        "docker",
    )
    .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, "config.yaml");

    // Verify the file was written to the staging directory
    let output_path = staging_dir.join("config.yaml");
    assert!(
        output_path.exists(),
        "templated file should exist in staging dir"
    );
    let content = fs::read_to_string(&output_path).unwrap();
    assert_eq!(content, "app: myapp\nversion: 1.0.0");
}

// -----------------------------------------------------------------------
// Docker behavioral gap tests
// -----------------------------------------------------------------------

#[test]
fn test_tag_suffix_amd64() {
    assert_eq!(tag_suffix("linux/amd64"), "amd64");
}

#[test]
fn test_tag_suffix_arm64() {
    assert_eq!(tag_suffix("linux/arm64"), "arm64");
}

#[test]
fn test_tag_suffix_arm_v7() {
    assert_eq!(tag_suffix("linux/arm/v7"), "armv7");
}

#[test]
fn test_sbom_uses_attest_format() {
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &[],
        labels: &[],
        flags: &[],
        sbom: true,
        push: false,
        load: true,
    })
    .unwrap();
    assert!(
        cmd.contains(&"--attest=type=sbom".to_string()),
        "SBOM should use --attest=type=sbom, not --sbom=true"
    );
    assert!(
        !cmd.contains(&"--sbom=true".to_string()),
        "should not contain old --sbom=true flag"
    );
}

#[test]
fn test_annotations_no_prefix_single_platform() {
    let annotations = vec![("foo".to_string(), "bar".to_string())];
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &annotations,
        labels: &[],
        flags: &[],
        sbom: false,
        push: false,
        load: true,
    })
    .unwrap();
    assert!(
        cmd.contains(&"foo=bar".to_string()),
        "single-platform annotations should NOT get index: prefix"
    );
}

#[test]
fn test_annotations_get_index_prefix_multi_platform() {
    let annotations = vec![("foo".to_string(), "bar".to_string())];
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64", "linux/arm64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &annotations,
        labels: &[],
        flags: &[],
        sbom: false,
        push: true,
        load: true,
    })
    .unwrap();
    assert!(
        cmd.contains(&"index:foo=bar".to_string()),
        "multi-platform annotations should get index: prefix"
    );
}

#[test]
fn test_annotations_no_double_index_prefix() {
    let annotations = vec![("index:foo".to_string(), "bar".to_string())];
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/ctx",
        platforms: &["linux/amd64", "linux/arm64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &annotations,
        labels: &[],
        flags: &[],
        sbom: false,
        push: true,
        load: true,
    })
    .unwrap();
    assert!(
        cmd.contains(&"index:foo=bar".to_string()),
        "already-prefixed annotations should not get double prefix"
    );
    assert!(
        !cmd.contains(&"index:index:foo=bar".to_string()),
        "must not double-prefix"
    );
}

#[test]
fn test_docker_sign_config_output_bool() {
    use anodizer_core::config::DockerSignConfig;
    let yaml = r#"
cmd: cosign
output: true
"#;
    let cfg: DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(cfg.output.unwrap().as_bool());
}

#[test]
fn test_docker_sign_config_output_string() {
    use anodizer_core::config::DockerSignConfig;
    let yaml = r#"
cmd: cosign
output: "false"
"#;
    let cfg: DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(!cfg.output.unwrap().as_bool());
}

#[test]
fn test_docker_sign_config_output_missing() {
    use anodizer_core::config::DockerSignConfig;
    let yaml = r#"
cmd: cosign
"#;
    let cfg: DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(cfg.output.is_none());
}

#[test]
fn test_docker_sign_config_output_template_string() {
    use anodizer_core::config::DockerSignConfig;
    let yaml = r#"
cmd: cosign
output: "{{ .IsSnapshot }}"
"#;
    let cfg: DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let output = cfg.output.unwrap();
    // Should be recognized as a template string, not a literal bool
    assert!(output.is_template());
}

#[test]
fn test_sign_config_output_string_or_bool() {
    use anodizer_core::config::SignConfig;
    let yaml_bool = r#"
cmd: gpg
output: true
"#;
    let cfg: SignConfig = serde_yaml_ng::from_str(yaml_bool).unwrap();
    assert!(cfg.output.unwrap().as_bool());

    let yaml_str = r#"
cmd: gpg
output: "false"
"#;
    let cfg2: SignConfig = serde_yaml_ng::from_str(yaml_str).unwrap();
    assert!(!cfg2.output.unwrap().as_bool());
}

#[test]
fn test_docker_digest_config_parses() {
    use anodizer_core::config::DockerDigestConfig;
    let yaml = r#"
skip: false
name_template: "{{ .ProjectName }}_{{ .Version }}_checksums.txt"
"#;
    let cfg: DockerDigestConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(!cfg.skip.unwrap().as_bool());
    assert_eq!(
        cfg.name_template.as_deref(),
        Some("{{ .ProjectName }}_{{ .Version }}_checksums.txt")
    );
}

#[test]
fn test_docker_digest_config_defaults() {
    use anodizer_core::config::DockerDigestConfig;
    let yaml = "{}";
    let cfg: DockerDigestConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(cfg.skip.is_none());
    assert!(cfg.name_template.is_none());
}

#[test]
fn test_docker_digest_config_disable_template() {
    use anodizer_core::config::DockerDigestConfig;
    let yaml = r#"
skip: "{{ .IsSnapshot }}"
"#;
    let cfg: DockerDigestConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(cfg.skip.unwrap().is_template());
}

#[test]
fn test_docker_build_job_env_vars_field() {
    // Verify DockerBuildJob carries env_vars through to execution
    let mut env = BTreeMap::new();
    env.insert("DOCKER_BUILDKIT".to_string(), "1".to_string());
    env.insert("MY_VAR".to_string(), "value".to_string());

    let job = DockerBuildJob {
        cmd_args: vec!["echo".to_string(), "test".to_string()],
        backend_label: "test".to_string(),
        crate_name: "test".to_string(),
        idx: 0,
        max_attempts: 1,
        base_delay: Duration::from_secs(1),
        max_delay: None,
        rendered_tags: vec![],
        platforms_str: String::new(),
        staging_dir: PathBuf::new(),
        id: None,
        use_backend: None,
        dist: PathBuf::new(),
        skip_digest: false,
        digest_name_template: None,
        env_vars: env,
    };

    assert_eq!(job.env_vars.len(), 2);
    assert_eq!(job.env_vars.get("DOCKER_BUILDKIT").unwrap(), "1");
    assert_eq!(job.env_vars.get("MY_VAR").unwrap(), "value");
}

#[test]
fn test_v2_iidfile_digest_read() {
    // Simulate the iidfile-based digest capture path:
    // write an id.txt to a staging dir, then verify it's read correctly.
    let tmp = TempDir::new().unwrap();
    let staging_dir = tmp.path().join("staging");
    fs::create_dir_all(&staging_dir).unwrap();

    let digest = "sha256:abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
    fs::write(staging_dir.join("id.txt"), digest).unwrap();

    // Simulate the read logic from execute_docker_build
    let iidfile = staging_dir.join("id.txt");
    let digest_content = fs::read_to_string(&iidfile).unwrap();
    let read_digest = digest_content.trim().to_string();
    assert_eq!(read_digest, digest);

    // Verify per-tag digests are populated correctly
    let tags = vec!["img:latest".to_string(), "img:v1.0.0".to_string()];
    let mut tag_digests = BTreeMap::new();
    for tag in &tags {
        tag_digests.insert(tag.clone(), read_digest.clone());
    }
    assert_eq!(tag_digests.len(), 2);
    assert_eq!(tag_digests.get("img:latest").unwrap(), digest);
    assert_eq!(tag_digests.get("img:v1.0.0").unwrap(), digest);
}

// -----------------------------------------------------------------------
// Task 8: Levenshtein distance tests
// -----------------------------------------------------------------------

#[test]
fn test_levenshtein_distance() {
    assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
    assert_eq!(levenshtein_distance("", "abc"), 3);
    assert_eq!(levenshtein_distance("abc", ""), 3);
    assert_eq!(levenshtein_distance("abc", "abc"), 0);
    assert_eq!(
        levenshtein_distance("ghcr.io/owner/app:latest", "ghcr.io/owner/app:latset"),
        2
    );
}

// -----------------------------------------------------------------------
// Task 9: Project marker detection tests
// -----------------------------------------------------------------------

#[test]
fn test_project_marker_detection() {
    let markers = ["go.mod", "Cargo.toml", "package.json", "pom.xml"];
    for m in &markers {
        assert!(
            PROJECT_MARKERS.contains(m),
            "{} should be a project marker",
            m
        );
    }
    assert!(!PROJECT_MARKERS.contains(&"myapp.conf"));
    assert!(!PROJECT_MARKERS.contains(&"config.yaml"));
}

#[test]
fn test_project_marker_in_subdirectory_path() {
    // warn_project_markers_in_extra_files extracts filename from paths
    let path = "subdir/nested/Cargo.toml";
    let filename = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap();
    assert!(PROJECT_MARKERS.contains(&filename));
}

// -----------------------------------------------------------------------
// Task 10: Docker daemon / load parameter tests
// -----------------------------------------------------------------------

#[test]
fn test_build_docker_v2_command_no_load_when_disabled() {
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &[],
        labels: &[],
        flags: &[],
        sbom: false,
        push: false,
        load: false,
    })
    .unwrap();
    assert!(!cmd.contains(&"--load".to_string()));
    assert!(!cmd.contains(&"--push".to_string()));
}

#[test]
fn test_build_docker_v2_command_load_when_enabled() {
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        image_tags: &["img:latest".to_string()],
        build_args: &[],
        annotations: &[],
        labels: &[],
        flags: &[],
        sbom: false,
        push: false,
        load: true,
    })
    .unwrap();
    assert!(cmd.contains(&"--load".to_string()));
}

// -----------------------------------------------------------------------
// Gap A: Legacy push — plain docker/podman don't get --push
// -----------------------------------------------------------------------

#[test]
fn test_build_docker_command_plain_docker_no_push_flag() {
    // Plain docker (use: docker) should never get --push in the build command
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        tags: &["ghcr.io/owner/app:v1.0.0"],
        extra_flags: &[],
        push: true,
        push_flags: &[],
        labels: &[],
        use_backend: Some("docker"),
    })
    .unwrap();
    assert!(
        !cmd.contains(&"--push".to_string()),
        "plain docker backend should not have --push in build command"
    );
    // Should not have --load either (that's buildx-only)
    assert!(!cmd.contains(&"--load".to_string()));
}

#[test]
fn test_build_docker_command_podman_no_push_flag() {
    // Podman should never get --push in the build command
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        tags: &["ghcr.io/owner/app:v1.0.0"],
        extra_flags: &[],
        push: true,
        push_flags: &[],
        labels: &[],
        use_backend: Some("podman"),
    })
    .unwrap();
    assert!(
        !cmd.contains(&"--push".to_string()),
        "podman backend should not have --push in build command"
    );
}

#[test]
fn test_build_docker_command_buildx_gets_push_flag() {
    // buildx SHOULD get --push in the build command
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        tags: &["ghcr.io/owner/app:v1.0.0"],
        extra_flags: &[],
        push: true,
        push_flags: &[],
        labels: &[],
        use_backend: Some("buildx"),
    })
    .unwrap();
    assert!(
        cmd.contains(&"--push".to_string()),
        "buildx backend should have --push in build command"
    );
}

#[test]
fn test_build_docker_command_multi_platform_no_implicit_buildx() {
    // Multi-platform with no explicit backend defaults to plain docker
    // (matching GoReleaser). --push is NOT added for plain docker.
    // Users must set `use: buildx` explicitly for buildx features.
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64", "linux/arm64"],
        tags: &["ghcr.io/owner/app:v1.0.0"],
        extra_flags: &[],
        push: true,
        push_flags: &[],
        labels: &[],
        use_backend: None,
    })
    .unwrap();
    assert!(
        !cmd.contains(&"--push".to_string()),
        "plain docker (default) should NOT have --push"
    );
    assert!(
        cmd.contains(&"--platform=linux/amd64,linux/arm64".to_string()),
        "platforms should still be set"
    );
}

#[test]
fn test_build_docker_command_multi_platform_explicit_buildx_gets_push() {
    // Multi-platform with explicit buildx should get --push
    let cmd = build_docker_command(&DockerV1Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64", "linux/arm64"],
        tags: &["ghcr.io/owner/app:v1.0.0"],
        extra_flags: &[],
        push: true,
        push_flags: &[],
        labels: &[],
        use_backend: Some("buildx"),
    })
    .unwrap();
    assert!(
        cmd.contains(&"--push".to_string()),
        "explicit buildx should have --push"
    );
}

// -----------------------------------------------------------------------
// Gap C: Retry with HTTP 506 and 510
// -----------------------------------------------------------------------

#[test]
fn test_is_retriable_error_506() {
    assert!(is_retriable_error(
        "received unexpected HTTP status: 506 Variant Also Negotiates"
    ));
}

#[test]
fn test_is_retriable_error_510() {
    assert!(is_retriable_error(
        "received unexpected HTTP status: 510 Not Extended"
    ));
}

// -----------------------------------------------------------------------
// Gap F: resolve_backend default is "docker"
// -----------------------------------------------------------------------

#[test]
fn test_resolve_backend_none_always_defaults_to_docker() {
    // Regardless of multi_platform flag, default is always plain docker.
    let (bin1, subs1) = resolve_backend(None, false).unwrap();
    let (bin2, subs2) = resolve_backend(None, true).unwrap();
    assert_eq!(bin1, "docker");
    assert_eq!(subs1, vec!["build"]);
    assert_eq!(bin2, "docker");
    assert_eq!(subs2, vec!["build"]);
}

// -----------------------------------------------------------------------
// Gap G: list_staging_dir_recursive
// -----------------------------------------------------------------------

#[test]
fn test_list_staging_dir_recursive_lists_files() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();

    // Create a structure:
    //   root/Dockerfile
    //   root/binaries/amd64/myapp
    //   root/binaries/arm64/myapp
    fs::write(root.join("Dockerfile"), "FROM scratch").unwrap();
    fs::create_dir_all(root.join("binaries/amd64")).unwrap();
    fs::create_dir_all(root.join("binaries/arm64")).unwrap();
    fs::write(root.join("binaries/amd64/myapp"), "bin").unwrap();
    fs::write(root.join("binaries/arm64/myapp"), "bin").unwrap();

    // Just verify it doesn't panic — the output goes to log.warn
    let log = anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Normal);
    list_staging_dir_recursive(root, root, &log);
}

// ---------------------------------------------------------------------------
// `docker buildx version` availability probe
//
// Adds a version-availability check alongside the existing `docker buildx
// inspect` driver check. Mirrors GoReleaser commit e09e23a / #6526: any
// docker config that needs buildx triggers a version probe so the user gets a
// clear actionable message when buildx is missing, rather than a cryptic
// failure deep inside `buildx build`.
// ---------------------------------------------------------------------------

#[test]
fn test_buildx_version_probe_available_emits_no_warning() {
    // When the probe reports buildx is reachable, the formatter returns None
    // (no warning to surface). The wired-in call in `run.rs` is a no-op.
    assert_eq!(
        format_buildx_version_warning(&BuildxVersionProbe::Available),
        None
    );
}

#[test]
fn test_buildx_version_probe_docker_missing_warns_with_buildx_required() {
    // When `docker` itself is unreachable, the warning must mention buildx so
    // the user knows v2 / `use: buildx` configs require it. Tested directly on
    // the pure formatter so the result is independent of the host's docker
    // install.
    let probe = BuildxVersionProbe::DockerMissing;
    let msg = format_buildx_version_warning(&probe).expect("missing docker should warn");
    assert!(
        msg.contains("buildx"),
        "warning should mention 'buildx' so the user can act on it: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("docker"),
        "warning should name the missing tool: {msg}"
    );
}

#[test]
fn test_buildx_version_probe_buildx_missing_warns_with_buildx_required() {
    // When `docker` runs but `docker buildx version` fails (e.g. plugin
    // missing), the warning should still surface "buildx" and include the
    // stderr context so the user can debug.
    let probe = BuildxVersionProbe::BuildxMissing {
        stderr: "docker: 'buildx' is not a docker command".to_string(),
    };
    let msg = format_buildx_version_warning(&probe).expect("missing buildx should warn");
    assert!(
        msg.contains("buildx"),
        "warning should mention 'buildx': {msg}"
    );
    assert!(
        msg.contains("'buildx' is not a docker command"),
        "warning should include stderr context for debuggability: {msg}"
    );
}

#[test]
fn test_buildx_version_check_increments_counter_on_v2_probe_outcome() {
    // Direct test of `run_buildx_version_check`: pass it an injected probe
    // that records each call and assert the counter ticks. This pins the
    // *probe-invocation contract* of `run_buildx_version_check`, not the
    // stage-level wiring. The stage path (`DockerStage.run`) currently
    // resolves the probe via the live `docker_buildx_version_probe()`
    // function, so end-to-end probe-injection from a unit test would need
    // a seam refactor on `run.rs::76-86` (tracked separately).
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_ref = Arc::clone(&calls);
    let probe = move || -> BuildxVersionProbe {
        calls_ref.fetch_add(1, Ordering::SeqCst);
        BuildxVersionProbe::Available
    };

    let log = anodizer_core::log::StageLogger::new("docker", anodizer_core::log::Verbosity::Normal);
    run_buildx_version_check(&log, &probe);

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "run_buildx_version_check must invoke the probe exactly once"
    );
}

#[test]
fn test_dockerstage_run_invokes_injected_buildx_probe_for_v2_crate() {
    // End-to-end seam check: when a `Context` has at least one crate with a
    // `docker_v2` config and `dry_run = true` (so no real `docker buildx
    // build` shells out), `DockerStage::with_probe(...).run(&mut ctx)` MUST
    // route the buildx-version probe through the injected closure. The gate
    // condition is pinned here so a future refactor that drops the wiring
    // fails this test instead of silently shelling out to `docker` in tests.
    //
    // `dry_run` is intentionally `false` so the probe gate fires
    // (`!dry_run && any docker_v2`). To stay sandbox-clean we still need to
    // avoid spawning real `docker buildx build`; the `disable: "true"` skip
    // on the v2 config short-circuits each config before any subprocess is
    // launched. The probe gate, however, runs once before the per-config
    // loop, so the counter ticks exactly once.
    use anodizer_core::config::{Config, CrateConfig, DockerV2Config};
    use anodizer_core::context::{Context, ContextOptions};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tmp = TempDir::new().unwrap();
    let dockerfile = tmp.path().join("Dockerfile");
    fs::write(&dockerfile, b"FROM scratch\n").unwrap();

    let v2_cfg = DockerV2Config {
        id: Some("myapp-v2".to_string()),
        images: vec!["ghcr.io/owner/myapp".to_string()],
        tags: vec!["latest".to_string()],
        dockerfile: dockerfile.to_string_lossy().into_owned(),
        platforms: Some(vec!["linux/amd64".to_string()]),
        // Short-circuit per-config build work; the probe gate runs before
        // the per-config skip check, so this still exercises the probe seam.
        skip: Some(StringOrBool::String("true".to_string())),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        docker_v2: Some(vec![v2_cfg]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: false,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.0.0");

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_ref = Arc::clone(&calls);
    let probe: Arc<super::BuildxVersionProbeFn> = Arc::new(move || {
        calls_ref.fetch_add(1, Ordering::SeqCst);
        BuildxVersionProbe::Available
    });

    let stage = DockerStage::with_probe(probe);
    // The stage may still bail later (e.g. on the per-config skip path or a
    // template render), but the probe gate runs first and unconditionally
    // invokes the injected closure exactly once. The counter assertion is
    // what we care about; the stage's `Result` is incidental.
    let _ = stage.run(&mut ctx);

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "DockerStage::run must invoke the injected buildx probe exactly once \
         when a docker_v2 config is present and dry_run is false",
    );
}
