use super::*;
use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{ChangelogConfig, CrateConfig, SignConfig};
use anodizer_core::config::{Config, ForceTokenKind, WorkspaceConfig};
use anodizer_core::context::Context;
use anodizer_core::context::ContextOptions;
use anodizer_core::log::StageLogger;
use anodizer_core::scm::ScmTokenType;
use std::path::{Path, PathBuf};

/// `Config.variables` is stored as a `BTreeMap` so iteration is
/// always sorted by key. The determinism harness fingerprints
/// `dist/config.yaml`, so two runs in the same workspace must emit
/// byte-identical YAML. `write_effective_config` is expected to route
/// the serialized config through `sort_yaml_mapping`, alphabetising the
/// keys of every mapping (top-level AND nested). Without that, the
/// `variables:` block's emit order drifts even though the source map
/// is sorted.
#[test]
fn write_effective_config_emits_sorted_keys() {
    use std::collections::BTreeMap;
    let tmp = tempfile::tempdir().unwrap();
    let mut variables = BTreeMap::new();
    // Insert in deliberately non-alphabetical order — the BTreeMap's
    // sorted iteration normalises this for the input side; the test
    // still verifies that `sort_yaml_mapping` sorts NESTED maps too.
    variables.insert("zeta".to_string(), "1".to_string());
    variables.insert("alpha".to_string(), "2".to_string());
    variables.insert("mu".to_string(), "3".to_string());
    variables.insert("beta".to_string(), "4".to_string());
    variables.insert("nu".to_string(), "5".to_string());
    let config = Config {
        project_name: "anodize".to_string(),
        dist: tmp.path().to_path_buf(),
        variables: Some(variables),
        ..Default::default()
    };
    let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);

    let mut variables_reversed = BTreeMap::new();
    for key in ["nu", "beta", "mu", "alpha", "zeta"] {
        let v = match key {
            "zeta" => "1",
            "alpha" => "2",
            "mu" => "3",
            "beta" => "4",
            "nu" => "5",
            _ => unreachable!(),
        };
        variables_reversed.insert(key.to_string(), v.to_string());
    }
    let config_reversed = Config {
        variables: Some(variables_reversed),
        ..config.clone()
    };

    write_effective_config(&config, &log).expect("first write");
    let yaml1 = std::fs::read_to_string(tmp.path().join("config.yaml")).unwrap();
    // Second write into the same dist with reversed-insertion variables.
    write_effective_config(&config_reversed, &log).expect("second write");
    let yaml2 = std::fs::read_to_string(tmp.path().join("config.yaml")).unwrap();
    assert_eq!(
        yaml1, yaml2,
        "two write_effective_config calls with identical input keys \
             must produce byte-identical YAML regardless of HashMap \
             insertion order (HashMap-iteration drift would fail this)"
    );

    // And the variables block keys must be alphabetical.
    let var_block_lines: Vec<&str> = yaml1
        .lines()
        .skip_while(|l| !l.starts_with("variables:"))
        .skip(1)
        .take_while(|l| l.starts_with("  ") || l.starts_with('\t'))
        .collect();
    let keys: Vec<&str> = var_block_lines
        .iter()
        .filter_map(|l| l.trim().split(':').next())
        .collect();
    assert_eq!(
        keys,
        vec!["alpha", "beta", "mu", "nu", "zeta"],
        "variables: keys must be emitted in alphabetical order; got {:?} \
             from yaml:\n{}",
        keys,
        yaml1,
    );
}

/// Recursive guard: the harness's drift channel is most often a *nested*
/// HashMap (e.g. `docker.labels`, `nfpm.dependencies`,
/// `announce.<flavour>.headers`). `sort_yaml_mapping` must walk into
/// sub-mappings AND into sequences-of-mappings. Hand-crafted
/// `serde_yaml_ng::Value` to exercise both axes.
#[test]
fn sort_yaml_mapping_recurses_into_nested_maps_and_sequences() {
    let yaml = "\
top:
  z: 1
  a: 2
list:
  - inner_z: 1
    inner_a: 2
  - solo: 3
";
    let mut value: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).unwrap();
    sort_yaml_mapping(&mut value);
    let out = serde_yaml_ng::to_string(&value).unwrap();
    // Top-level keys: list comes before top alphabetically.
    let first_line = out.lines().next().unwrap();
    assert!(
        first_line.starts_with("list:"),
        "top-level keys must be sorted alphabetically; got {out:?}"
    );
    // Sub-mapping under `top:` must be sorted (a before z).
    let top_pos = out.find("top:").unwrap();
    let top_block = &out[top_pos..];
    let a_pos = top_block.find("a:").expect("a: present");
    let z_pos = top_block.find("z:").expect("z: present");
    assert!(
        a_pos < z_pos,
        "nested mapping under `top:` must be sorted; got {out:?}"
    );
    // Sub-mapping inside the first list element must also be sorted.
    let list_pos = out.find("list:").unwrap();
    let list_block = &out[list_pos..];
    let inner_a = list_block.find("inner_a:").expect("inner_a: present");
    let inner_z = list_block.find("inner_z:").expect("inner_z: present");
    assert!(
        inner_a < inner_z,
        "nested mapping inside a sequence element must be sorted; got {out:?}"
    );
}

fn make_crate(name: &str) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some(format!("{}-v{{{{ .Version }}}}", name)),
        ..Default::default()
    }
}

#[test]
fn test_apply_workspace_overlay_replaces_crates() {
    let mut config = Config {
        project_name: "test".to_string(),
        crates: vec![make_crate("original")],
        ..Default::default()
    };
    let ws = WorkspaceConfig {
        name: "ws".to_string(),
        crates: vec![make_crate("ws-crate")],
        ..Default::default()
    };

    apply_workspace_overlay(&mut config, &ws);
    assert_eq!(config.crates.len(), 1);
    assert_eq!(config.crates[0].name, "ws-crate");
}

#[test]
fn test_apply_workspace_overlay_clears_workspaces() {
    // The overlaid run IS the chosen workspace: sibling workspaces must
    // drop out of the universe, or every stage's "empty selection = all"
    // walk (and `check config --workspace X`) would still see sibling
    // crates under X's overlay.
    let ws_a = WorkspaceConfig {
        name: "ws-a".to_string(),
        crates: vec![make_crate("a-crate")],
        ..Default::default()
    };
    let ws_b = WorkspaceConfig {
        name: "ws-b".to_string(),
        crates: vec![make_crate("b-crate")],
        ..Default::default()
    };
    let mut config = Config {
        project_name: "test".to_string(),
        workspaces: Some(vec![ws_a.clone(), ws_b]),
        ..Default::default()
    };

    apply_workspace_overlay(&mut config, &ws_a);
    assert!(
        config.workspaces.is_none(),
        "overlay must clear config.workspaces"
    );
    let universe: Vec<&str> = config
        .crate_universe()
        .into_iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(
        universe,
        vec!["a-crate"],
        "post-overlay universe must contain ONLY the chosen workspace's crates"
    );
}

#[test]
fn test_workspace_containing_crate_resolves_and_shadows() {
    let config = Config {
        project_name: "test".to_string(),
        crates: vec![make_crate("top"), make_crate("shadowed")],
        workspaces: Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![make_crate("member"), make_crate("shadowed")],
            ..Default::default()
        }]),
        ..Default::default()
    };

    assert_eq!(
        workspace_containing_crate(&config, "member").map(|w| w.name.as_str()),
        Some("ws"),
        "workspace-only crate must resolve to its workspace"
    );
    assert!(
        workspace_containing_crate(&config, "top").is_none(),
        "top-level crate has no containing workspace"
    );
    assert!(
        workspace_containing_crate(&config, "shadowed").is_none(),
        "a top-level entry shadows a same-named workspace entry"
    );
    assert!(
        workspace_containing_crate(&config, "missing").is_none(),
        "unknown names resolve to no workspace"
    );
}

#[test]
fn test_apply_workspace_overlay_merges_env() {
    let mut config = Config {
        project_name: "test".to_string(),
        env: Some(vec![
            "SHARED=from-top".to_string(),
            "TOP_ONLY=top-value".to_string(),
        ]),
        ..Default::default()
    };
    let ws = WorkspaceConfig {
        name: "ws".to_string(),
        crates: vec![],
        env: Some(vec![
            "SHARED=from-ws".to_string(),
            "WS_ONLY=ws-value".to_string(),
        ]),
        ..Default::default()
    };

    apply_workspace_overlay(&mut config, &ws);
    let env = config.env.as_ref().unwrap();
    assert!(env.contains(&"TOP_ONLY=top-value".to_string()));
    assert!(env.contains(&"SHARED=from-ws".to_string()));
    assert!(env.contains(&"WS_ONLY=ws-value".to_string()));
}

#[test]
fn test_apply_workspace_overlay_replaces_signs() {
    let mut config = Config {
        project_name: "test".to_string(),
        signs: vec![SignConfig {
            cmd: Some("gpg".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let ws = WorkspaceConfig {
        name: "ws".to_string(),
        crates: vec![],
        signs: vec![SignConfig {
            cmd: Some("cosign".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };

    apply_workspace_overlay(&mut config, &ws);
    assert_eq!(config.signs.len(), 1);
    assert_eq!(config.signs[0].cmd.as_deref(), Some("cosign"));
}

#[test]
fn test_apply_workspace_overlay_replaces_changelog() {
    let mut config = Config {
        project_name: "test".to_string(),
        changelog: Some(ChangelogConfig {
            sort: Some("asc".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let ws = WorkspaceConfig {
        name: "ws".to_string(),
        crates: vec![],
        changelog: Some(ChangelogConfig {
            sort: Some("desc".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };

    apply_workspace_overlay(&mut config, &ws);
    assert_eq!(
        config.changelog.as_ref().unwrap().sort.as_deref(),
        Some("desc")
    );
}

#[test]
fn test_apply_workspace_overlay_skips_none_fields() {
    let mut config = Config {
        project_name: "test".to_string(),
        changelog: Some(ChangelogConfig {
            sort: Some("asc".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let ws = WorkspaceConfig {
        name: "ws".to_string(),
        crates: vec![],
        // changelog is None, should not overwrite
        ..Default::default()
    };

    apply_workspace_overlay(&mut config, &ws);
    // Original changelog preserved
    assert_eq!(
        config.changelog.as_ref().unwrap().sort.as_deref(),
        Some("asc")
    );
}

// -----------------------------------------------------------------------
// load_artifacts_from_dist tests
// -----------------------------------------------------------------------

#[test]
fn test_load_artifacts_from_dist_valid() {
    use anodizer_core::artifact::ArtifactKind;
    use anodizer_core::context::{Context, ContextOptions};

    let dir = tempfile::TempDir::new().unwrap();
    let artifacts_json = serde_json::json!([
        {
            "kind": "binary",
            "name": "myapp",
            "path": "dist/myapp",
            "target": "x86_64-unknown-linux-gnu",
            "crate_name": "myapp",
            "metadata": {},
            "size": 4096
        },
        {
            "kind": "archive",
            "name": "myapp.tar.gz",
            "path": "dist/myapp.tar.gz",
            "target": null,
            "crate_name": "myapp",
            "metadata": {"format": "tar.gz"}
        }
    ]);
    std::fs::write(
        dir.path().join("artifacts.json"),
        serde_json::to_string_pretty(&artifacts_json).unwrap(),
    )
    .unwrap();

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    load_artifacts_from_dist(&mut ctx, dir.path()).unwrap();

    let all = ctx.artifacts.all();
    assert_eq!(all.len(), 2);

    assert_eq!(all[0].kind, ArtifactKind::Binary);
    assert_eq!(all[0].name, "myapp");
    assert_eq!(
        all[0].size,
        Some(4096),
        "size should be preserved from JSON"
    );

    assert_eq!(all[1].kind, ArtifactKind::Archive);
    assert_eq!(all[1].name, "myapp.tar.gz");
    assert_eq!(
        all[1].metadata.get("format").map(|s| s.as_str()),
        Some("tar.gz")
    );
    assert_eq!(
        all[1].size, None,
        "size should be None when absent from JSON"
    );
}

#[test]
fn test_load_artifacts_from_dist_missing_file() {
    use anodizer_core::context::{Context, ContextOptions};

    let dir = tempfile::TempDir::new().unwrap();
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    let result = load_artifacts_from_dist(&mut ctx, dir.path());
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("no artifacts manifest found"),
        "error should mention missing file: {msg}"
    );
}

#[test]
fn test_load_artifacts_from_dist_invalid_json() {
    use anodizer_core::context::{Context, ContextOptions};

    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("artifacts.json"), "not valid json").unwrap();

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    let result = load_artifacts_from_dist(&mut ctx, dir.path());
    assert!(result.is_err());
}

#[test]
fn test_load_artifacts_from_dist_unknown_kind() {
    use anodizer_core::context::{Context, ContextOptions};

    let dir = tempfile::TempDir::new().unwrap();
    let artifacts_json = serde_json::json!([
        {
            "kind": "unknown_kind",
            "name": "thing",
            "path": "dist/thing",
            "target": null,
            "crate_name": "myapp",
            "metadata": {}
        }
    ]);
    std::fs::write(
        dir.path().join("artifacts.json"),
        serde_json::to_string_pretty(&artifacts_json).unwrap(),
    )
    .unwrap();

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    let result = load_artifacts_from_dist(&mut ctx, dir.path());
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("unknown artifact kind"),
        "error should mention unknown kind: {msg}"
    );
}

#[test]
fn test_load_artifacts_from_dist_roundtrip() {
    use anodizer_core::artifact::{Artifact, ArtifactKind, ArtifactRegistry};
    use anodizer_core::context::{Context, ContextOptions};

    // Build an artifact registry, serialize, write, then load back
    let mut registry = ArtifactRegistry::new();
    registry.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("dist/checksums.txt"),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: Some(256),
    });
    registry.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: std::path::PathBuf::from("dist/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let json_val = registry.to_artifacts_json().unwrap();
    let json_str = serde_json::to_string_pretty(&json_val).unwrap();

    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("artifacts.json"), &json_str).unwrap();

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    load_artifacts_from_dist(&mut ctx, dir.path()).unwrap();

    let loaded = ctx.artifacts.all();
    assert_eq!(loaded.len(), 2);

    // `to_artifacts_json` emits a stable sort on (kind, target,
    // crate_name, name, path) to keep `dist/artifacts.json` byte-
    // identical across runs regardless of registration order, so the
    // round-tripped order is Binary (kind="binary") before Checksum
    // (kind="checksum"), not the insertion order.
    assert_eq!(loaded[0].kind, ArtifactKind::Binary);
    assert_eq!(loaded[0].name, "myapp");
    assert_eq!(loaded[0].target.as_deref(), Some("aarch64-apple-darwin"));
    assert_eq!(loaded[0].size, None);

    assert_eq!(loaded[1].kind, ArtifactKind::Checksum);
    assert_eq!(loaded[1].name, "checksums.txt");
    assert_eq!(loaded[1].size, Some(256));
}

// -----------------------------------------------------------------------
// resolve_scm_token_type tests
// -----------------------------------------------------------------------

/// Build a `Context` whose `EnvSource` is a closed `MapEnvSource` carrying
/// the supplied `(key, value)` fixtures. Routes `resolve_scm_token_type`'s
/// env reads through the injected map so each test drives a hermetic
/// branch without touching process env.
fn ctx_with_env(config: &Config, env: &[(&str, &str)]) -> Context {
    ctx_with_env_inner(config, env, None)
}

fn ctx_with_env_and_cli_token(config: &Config, env: &[(&str, &str)], cli_token: &str) -> Context {
    ctx_with_env_inner(config, env, Some(cli_token.to_string()))
}

fn ctx_with_env_inner(config: &Config, env: &[(&str, &str)], cli_token: Option<String>) -> Context {
    let opts = ContextOptions {
        token: cli_token,
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), opts);
    let mut map = anodizer_core::env_source::MapEnvSource::new();
    for (k, v) in env {
        map.set(*k, *v);
    }
    ctx.set_env_source(map);
    ctx
}

#[test]
fn test_resolve_scm_token_type_default_is_github() {
    let config = Config::default();
    let mut ctx = ctx_with_env(&config, &[]);
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(ctx.token_type, ScmTokenType::GitHub);
    assert!(ctx.options.token.is_none());
}

#[test]
fn test_resolve_scm_token_type_force_gitlab() {
    let config = Config {
        force_token: Some(ForceTokenKind::GitLab),
        ..Default::default()
    };
    let mut ctx = ctx_with_env(&config, &[("GITLAB_TOKEN", "glpat-test123")]);
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(ctx.token_type, ScmTokenType::GitLab);
    assert_eq!(ctx.options.token.as_deref(), Some("glpat-test123"));
}

#[test]
fn test_resolve_scm_token_type_force_gitea() {
    let config = Config {
        force_token: Some(ForceTokenKind::Gitea),
        ..Default::default()
    };
    let mut ctx = ctx_with_env(&config, &[("GITEA_TOKEN", "gitea-tok")]);
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(ctx.token_type, ScmTokenType::Gitea);
    assert_eq!(ctx.options.token.as_deref(), Some("gitea-tok"));
}

#[test]
fn test_resolve_scm_token_type_env_gitlab_detected() {
    let config = Config::default();
    let mut ctx = ctx_with_env(&config, &[("GITLAB_TOKEN", "glpat-env")]);
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(ctx.token_type, ScmTokenType::GitLab);
    assert_eq!(ctx.options.token.as_deref(), Some("glpat-env"));
}

#[test]
fn test_resolve_scm_token_type_env_gitea_detected() {
    let config = Config::default();
    let mut ctx = ctx_with_env(&config, &[("GITEA_TOKEN", "gitea-env")]);
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(ctx.token_type, ScmTokenType::Gitea);
    assert_eq!(ctx.options.token.as_deref(), Some("gitea-env"));
}

#[test]
fn test_resolve_scm_token_type_github_token_from_env() {
    let config = Config::default();
    let mut ctx = ctx_with_env(&config, &[("GITHUB_TOKEN", "ghp-from-env")]);
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(ctx.token_type, ScmTokenType::GitHub);
    assert_eq!(ctx.options.token.as_deref(), Some("ghp-from-env"));
}

#[test]
fn test_resolve_scm_token_type_anodizer_github_token_takes_precedence() {
    let config = Config::default();
    let mut ctx = ctx_with_env(
        &config,
        &[
            ("ANODIZER_GITHUB_TOKEN", "anodizer-tok"),
            ("GITHUB_TOKEN", "gh-tok"),
        ],
    );
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(ctx.token_type, ScmTokenType::GitHub);
    assert_eq!(
        ctx.options.token.as_deref(),
        Some("anodizer-tok"),
        "ANODIZER_GITHUB_TOKEN should take precedence over GITHUB_TOKEN"
    );
}

#[test]
fn test_resolve_scm_token_type_cli_token_preserved() {
    let config = Config::default();
    let mut ctx = ctx_with_env_and_cli_token(&config, &[("GITHUB_TOKEN", "from-env")], "from-cli");
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(ctx.token_type, ScmTokenType::GitHub);
    assert_eq!(
        ctx.options.token.as_deref(),
        Some("from-cli"),
        "CLI --token flag should not be overwritten by env var"
    );
}

#[test]
fn test_resolve_scm_token_type_force_overrides_env_detection() {
    // GITLAB_TOKEN is set, but force_token says GitHub.
    let config = Config {
        force_token: Some(ForceTokenKind::GitHub),
        ..Default::default()
    };
    let mut ctx = ctx_with_env(&config, &[("GITLAB_TOKEN", "glpat-ignored")]);
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(
        ctx.token_type,
        ScmTokenType::GitHub,
        "force_token should override env-based detection"
    );
    assert!(
        ctx.options.token.is_none(),
        "no GitHub token env var set, so token should remain None"
    );
}

#[test]
fn test_resolve_scm_token_type_gitlab_priority_over_gitea() {
    let config = Config::default();
    let mut ctx = ctx_with_env(
        &config,
        &[("GITLAB_TOKEN", "gl-tok"), ("GITEA_TOKEN", "gt-tok")],
    );
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(
        ctx.token_type,
        ScmTokenType::GitLab,
        "GITLAB_TOKEN should be checked before GITEA_TOKEN"
    );
    assert_eq!(ctx.options.token.as_deref(), Some("gl-tok"));
}

#[test]
fn test_resolve_scm_token_type_anodizer_force_token_env_gitlab() {
    let config = Config::default();
    let mut ctx = ctx_with_env(
        &config,
        &[
            ("ANODIZER_FORCE_TOKEN", "gitlab"),
            ("GITLAB_TOKEN", "glpat-env"),
        ],
    );
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(
        ctx.token_type,
        ScmTokenType::GitLab,
        "ANODIZER_FORCE_TOKEN=gitlab should force GitLab"
    );
    assert_eq!(ctx.options.token.as_deref(), Some("glpat-env"));
}

#[test]
fn test_resolve_scm_token_type_anodizer_force_token_env_github() {
    let config = Config::default();
    let mut ctx = ctx_with_env(
        &config,
        &[
            ("ANODIZER_FORCE_TOKEN", "github"),
            ("GITLAB_TOKEN", "glpat-ignored"),
            ("GITHUB_TOKEN", "ghp-forced"),
        ],
    );
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(
        ctx.token_type,
        ScmTokenType::GitHub,
        "ANODIZER_FORCE_TOKEN=github should override GITLAB_TOKEN detection"
    );
    assert_eq!(ctx.options.token.as_deref(), Some("ghp-forced"));
}

#[test]
fn test_resolve_scm_token_type_goreleaser_force_token_compat() {
    let config = Config::default();
    let mut ctx = ctx_with_env(
        &config,
        &[
            ("GORELEASER_FORCE_TOKEN", "gitea"),
            ("GITEA_TOKEN", "gitea-compat"),
        ],
    );
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(
        ctx.token_type,
        ScmTokenType::Gitea,
        "GORELEASER_FORCE_TOKEN should work as compat fallback"
    );
    assert_eq!(ctx.options.token.as_deref(), Some("gitea-compat"));
}

#[test]
fn test_resolve_scm_token_type_anodizer_force_token_overrides_goreleaser() {
    let config = Config::default();
    let mut ctx = ctx_with_env(
        &config,
        &[
            ("ANODIZER_FORCE_TOKEN", "github"),
            ("GORELEASER_FORCE_TOKEN", "gitlab"),
            ("GITHUB_TOKEN", "ghp-wins"),
            ("GITLAB_TOKEN", "glpat-loses"),
        ],
    );
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(
        ctx.token_type,
        ScmTokenType::GitHub,
        "ANODIZER_FORCE_TOKEN should take precedence over GORELEASER_FORCE_TOKEN"
    );
    assert_eq!(ctx.options.token.as_deref(), Some("ghp-wins"));
}

#[test]
fn test_resolve_scm_token_type_config_force_token_overrides_env() {
    let config = Config {
        force_token: Some(ForceTokenKind::GitHub),
        ..Default::default()
    };
    let mut ctx = ctx_with_env(
        &config,
        &[
            ("ANODIZER_FORCE_TOKEN", "gitlab"),
            ("GITHUB_TOKEN", "ghp-config"),
        ],
    );
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(
        ctx.token_type,
        ScmTokenType::GitHub,
        "config.force_token should override ANODIZER_FORCE_TOKEN env var"
    );
    assert_eq!(ctx.options.token.as_deref(), Some("ghp-config"));
}

#[test]
fn test_resolve_scm_token_type_invalid_force_token_env_ignored() {
    let config = Config::default();
    let mut ctx = ctx_with_env(
        &config,
        &[
            ("ANODIZER_FORCE_TOKEN", "invalid"),
            ("GITLAB_TOKEN", "glpat-detected"),
        ],
    );
    resolve_scm_token_type(&mut ctx, &config);

    assert_eq!(
        ctx.token_type,
        ScmTokenType::GitLab,
        "invalid ANODIZER_FORCE_TOKEN should fall back to env detection"
    );
    assert_eq!(ctx.options.token.as_deref(), Some("glpat-detected"));
}

// ---- collect_build_targets override semantics ---------------------

#[test]
fn test_collect_build_targets_per_build_overrides_defaults() {
    use anodizer_core::config::{BuildConfig, Defaults};

    let config = Config {
        project_name: "test".to_string(),
        defaults: Some(Defaults {
            targets: Some(vec!["a".to_string(), "b".to_string()]),
            ..Default::default()
        }),
        crates: vec![CrateConfig {
            name: "k1".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ Version }}".to_string()),
            builds: Some(vec![BuildConfig {
                binary: Some("k1".to_string()),
                targets: Some(vec!["c".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let result = collect_build_targets(&config, &[]);
    assert_eq!(
        result,
        vec!["c".to_string()],
        "per-build targets should REPLACE defaults.targets, not concat",
    );
}

#[test]
fn test_collect_build_targets_per_build_none_falls_back_to_defaults() {
    use anodizer_core::config::{BuildConfig, Defaults};

    let config = Config {
        project_name: "test".to_string(),
        defaults: Some(Defaults {
            targets: Some(vec!["a".to_string(), "b".to_string()]),
            ..Default::default()
        }),
        crates: vec![CrateConfig {
            name: "k1".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ Version }}".to_string()),
            builds: Some(vec![BuildConfig {
                binary: Some("k1".to_string()),
                targets: None, // not set; should inherit defaults
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let result = collect_build_targets(&config, &[]);
    assert_eq!(
        result,
        vec!["a".to_string(), "b".to_string()],
        "build with targets=None should inherit defaults.targets",
    );
}

#[test]
fn test_collect_build_targets_no_bin_crate_contributes_nothing() {
    use anodizer_core::config::Defaults;

    // A crate with no `builds:` and no `--bin` named after itself (path="."
    // resolves to package "anodizer", not "lib") compiles nothing in the
    // planner, so it must contribute no targets — even though defaults.targets
    // is set. Reporting the defaults here would over-report against the build.
    let config = Config {
        project_name: "test".to_string(),
        defaults: Some(Defaults {
            targets: Some(vec!["a".to_string(), "b".to_string()]),
            ..Default::default()
        }),
        crates: vec![CrateConfig {
            name: "lib".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ Version }}".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(
        collect_build_targets(&config, &[]).is_empty(),
        "a no-bin/no-builds crate builds nothing, so contributes no targets",
    );
}

#[test]
fn test_collect_build_targets_unset_defaults_falls_back_to_canonical_set() {
    use anodizer_core::config::BuildConfig;

    // A producing build with targets=None and NO defaults.targets inherits
    // the canonical DEFAULT_TARGETS set the planner compiles over — not an
    // empty list. This is the fallback the cross-toolchain self-report and
    // host filter rely on. An explicit `binary` clears the compile/artifact
    // gate so the build produces (a `binary: None` build on a crate with no
    // `--bin` would correctly compile nothing under the planner's gate).
    let config = Config {
        project_name: "test".to_string(),
        crates: vec![CrateConfig {
            name: "k1".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ Version }}".to_string()),
            builds: Some(vec![BuildConfig {
                binary: Some("k1".to_string()),
                targets: None,
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let result = collect_build_targets(&config, &[]);
    let expected: Vec<String> = anodizer_core::target::DEFAULT_TARGETS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(
        result, expected,
        "targets=None with no defaults must fall back to DEFAULT_TARGETS",
    );
}

// ---- merge_env_with_defaults --------------------------------------

#[test]
fn test_merge_env_with_defaults_both_none_yields_none() {
    assert!(merge_env_with_defaults(None, None).is_none());
}

#[test]
fn test_merge_env_with_defaults_only_defaults_yields_defaults() {
    let d = vec!["FOO=defaults".to_string()];
    let merged = merge_env_with_defaults(Some(&d), None).unwrap();
    assert_eq!(merged, vec!["FOO=defaults".to_string()]);
}

#[test]
fn test_merge_env_with_defaults_only_config_yields_config() {
    let c = vec!["BAR=top".to_string()];
    let merged = merge_env_with_defaults(None, Some(&c)).unwrap();
    assert_eq!(merged, vec!["BAR=top".to_string()]);
}

#[test]
fn test_merge_env_with_defaults_disjoint_keys_concat() {
    // defaults.env contributes when no per-config entry shadows it.
    let d = vec!["FOO=defaults".to_string()];
    let c = vec!["BAR=top".to_string()];
    let merged = merge_env_with_defaults(Some(&d), Some(&c)).unwrap();
    assert_eq!(
        merged,
        vec!["FOO=defaults".to_string(), "BAR=top".to_string()]
    );
}

#[test]
fn test_merge_env_with_defaults_top_level_wins_on_collision() {
    // Defaults provide FOO=a, top-level overrides with FOO=b.
    // Order is defaults-first so the per-key last-write-wins inside
    // setup_env produces FOO=b.
    let d = vec!["FOO=a".to_string()];
    let c = vec!["FOO=b".to_string()];
    let merged = merge_env_with_defaults(Some(&d), Some(&c)).unwrap();
    // Both entries appear; the consumer (setup_env) iterates in order
    // and the last write to a key wins.
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0], "FOO=a");
    assert_eq!(merged[1], "FOO=b");
}

// ---- defaults.env wired into setup_env ------------------------------

use anodizer_core::config::Defaults;

/// `setup_env` mutates process env via the load-bearing
/// `set_env_var_single_threaded` path so child commands (docker /
/// rustup / git hooks) inherit user-supplied entries. These two
/// tests assert the template-context wiring only — they never
/// observe the process-env side effect, and the fixture keys
/// (`DEFAULTS_ENV_*`) are uniquely shaped so accidental cross-test
/// reads of the same key are vanishingly unlikely.
#[test]
fn test_setup_env_inherits_defaults_env_when_crate_unset() {
    let config = Config {
        defaults: Some(Defaults {
            env: Some(vec!["DEFAULTS_ENV_INHERITED=defaults".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ContextOptions::default());
    let log = anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
    setup_env(&mut ctx, &config, &log).expect("setup_env should succeed");
    assert_eq!(
        ctx.template_vars()
            .all_config_env()
            .get("DEFAULTS_ENV_INHERITED")
            .map(|s| s.as_str()),
        Some("defaults"),
        "defaults.env entry should populate the template context",
    );
}

#[test]
fn test_setup_env_top_level_env_wins_over_defaults_env() {
    let config = Config {
        defaults: Some(Defaults {
            env: Some(vec!["DEFAULTS_ENV_OVERRIDE=a".to_string()]),
            ..Default::default()
        }),
        env: Some(vec!["DEFAULTS_ENV_OVERRIDE=b".to_string()]),
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ContextOptions::default());
    let log = anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
    setup_env(&mut ctx, &config, &log).expect("setup_env should succeed");
    assert_eq!(
        ctx.template_vars()
            .all_config_env()
            .get("DEFAULTS_ENV_OVERRIDE")
            .map(|s| s.as_str()),
        Some("b"),
        "top-level config.env should override defaults.env on duplicate key",
    );
}

/// Strict variable rendering — a template typo (`{{ .Tagg }}` instead
/// of `{{ .Tag }}`) used to silently pass the literal string through
/// to downstream publishers; the strict path makes it a hard error so
/// the user sees the failure at config-load.
#[test]
fn test_setup_env_variables_template_error_fails_load() {
    use std::collections::BTreeMap;
    let mut vars = BTreeMap::new();
    vars.insert(
        "bad".to_string(),
        "{{ NoSuchVariable | nonexistent_filter }}".to_string(),
    );
    let config = Config {
        variables: Some(vars),
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ContextOptions::default());
    let log = anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
    let err = setup_env(&mut ctx, &config, &log)
        .expect_err("invalid variable template must fail the load");
    let msg = format!("{:#}", err);
    assert!(
        msg.contains("variables.bad"),
        "error should name the offending variable key, got: {msg}"
    );
}

/// When `ANODIZER_CURRENT_TAG` and `GORELEASER_CURRENT_TAG` are absent and
/// `GITHUB_REF_TYPE=tag`, the override must resolve to `GITHUB_REF_NAME`.
/// This guards the Release.yml path where GHA sets only the standard
/// `GITHUB_REF_*` vars and neither anodizer-specific var is exported.
#[test]
fn resolve_git_context_github_ref_name_fallback_fires_when_anodizer_tags_unset() {
    let config = Config {
        project_name: "test".to_string(),
        ..Default::default()
    };
    let ctx = ctx_with_env(
        &config,
        &[("GITHUB_REF_TYPE", "tag"), ("GITHUB_REF_NAME", "v1.2.3")],
    );
    let tag_override = resolve_tag_override(
        ctx.env_var("ANODIZER_CURRENT_TAG"),
        ctx.env_var("GORELEASER_CURRENT_TAG"),
        ctx.env_var("GITHUB_REF_TYPE"),
        ctx.env_var("GITHUB_REF_NAME"),
    );
    assert_eq!(
        tag_override.as_deref(),
        Some("v1.2.3"),
        "GITHUB_REF_NAME fallback must fire when anodizer/goreleaser tag vars are absent"
    );
}

/// When `GITHUB_REF_TYPE` is not `tag` (e.g. `branch`), the
/// `GITHUB_REF_NAME` fallback must NOT fire — branch names are not tags.
#[test]
fn resolve_git_context_github_ref_name_fallback_skipped_for_branch_push() {
    let config = Config {
        project_name: "test".to_string(),
        ..Default::default()
    };
    let ctx = ctx_with_env(
        &config,
        &[("GITHUB_REF_TYPE", "branch"), ("GITHUB_REF_NAME", "master")],
    );
    let tag_override = resolve_tag_override(
        ctx.env_var("ANODIZER_CURRENT_TAG"),
        ctx.env_var("GORELEASER_CURRENT_TAG"),
        ctx.env_var("GITHUB_REF_TYPE"),
        ctx.env_var("GITHUB_REF_NAME"),
    );
    assert!(
        tag_override.is_none(),
        "GITHUB_REF_NAME must not be used as tag override when GITHUB_REF_TYPE=branch"
    );
}

/// Deterministic order — a value referencing an earlier-sorting key
/// resolves correctly because the BTreeMap iterates in alphabetical
/// order. (`b` references `a`; `a` sorts first, so `b` sees `a`.)
#[test]
fn test_setup_env_variables_resolve_in_sorted_order() {
    use std::collections::BTreeMap;
    let mut vars = BTreeMap::new();
    // Insert in reverse order to confirm BTreeMap iteration order
    // (not insertion order) drives resolution.
    vars.insert("b".to_string(), "{{ Var.a }}_v2".to_string());
    vars.insert("a".to_string(), "hello".to_string());
    let config = Config {
        project_name: "p".to_string(),
        variables: Some(vars),
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ContextOptions::default());
    let log = anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
    setup_env(&mut ctx, &config, &log).expect("setup_env should succeed");
    // `b` references `a` and `a` sorts first, so the resolved value
    // for `b` is `hello_v2`.
    let rendered = ctx.render_template("{{ Var.b }}").expect("render Var.b");
    assert_eq!(rendered, "hello_v2");
}

/// A forward reference (a value referencing a sibling key that sorts LATER)
/// renders against an unset `.Var.<name>`. When the operator guards it with
/// `| default(value="")` it silently yields empty — `setup_env` must warn so
/// the blank substitution isn't a surprise.
#[test]
fn test_setup_env_variables_forward_reference_warns() {
    use std::collections::BTreeMap;
    let mut vars = BTreeMap::new();
    // `a` references `z`, which sorts AFTER `a`, so `z` is still unset when
    // `a` renders. The `default` filter swallows the missing-key error.
    vars.insert(
        "a".to_string(),
        "{{ Var.z | default(value=\"\") }}".to_string(),
    );
    vars.insert("z".to_string(), "later".to_string());
    let config = Config {
        project_name: "p".to_string(),
        variables: Some(vars),
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ContextOptions::default());
    let (log, capture) =
        anodizer_core::log::StageLogger::with_capture("test", anodizer_core::log::Verbosity::Quiet);
    setup_env(&mut ctx, &config, &log).expect("setup_env should succeed");
    let warnings = capture.warn_messages();
    assert!(
        warnings
            .iter()
            .any(|m| m.contains("variables.a") && m.contains('z') && m.contains("defined later")),
        "forward reference must emit a warning naming the key and its later \
             dependency; got: {warnings:?}"
    );
}

/// The forward-ref scan must not warn on a backward reference (the common,
/// correct case): `b` references `a`, `a` sorts first, so it is already set.
#[test]
fn test_setup_env_variables_backward_reference_no_warn() {
    use std::collections::BTreeMap;
    let mut vars = BTreeMap::new();
    vars.insert("b".to_string(), "{{ Var.a }}".to_string());
    vars.insert("a".to_string(), "hello".to_string());
    let config = Config {
        project_name: "p".to_string(),
        variables: Some(vars),
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ContextOptions::default());
    let (log, capture) =
        anodizer_core::log::StageLogger::with_capture("test", anodizer_core::log::Verbosity::Quiet);
    setup_env(&mut ctx, &config, &log).expect("setup_env should succeed");
    assert_eq!(
        capture.warn_count(),
        0,
        "a backward reference (dependency sorts first) must not warn"
    );
}

// -----------------------------------------------------------------------
// parse_csv_list
// -----------------------------------------------------------------------

#[test]
fn parse_csv_list_none_passes_through() {
    assert_eq!(parse_csv_list(None, "--targets=<a,b>").unwrap(), None);
}

#[test]
fn parse_csv_list_splits_and_trims() {
    let got = parse_csv_list(Some(" a , b ,c"), "--targets=<a,b>")
        .unwrap()
        .unwrap();
    assert_eq!(got, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
}

#[test]
fn parse_csv_list_drops_empty_tokens_from_double_and_trailing_commas() {
    let got = parse_csv_list(Some("a,,b,"), "--stages=<x,y>")
        .unwrap()
        .unwrap();
    assert_eq!(got, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn parse_csv_list_all_empty_is_error_with_flag_help() {
    let err = parse_csv_list(Some("  , , "), "--targets=<a,b>").unwrap_err();
    assert!(
        err.starts_with("--targets=<a,b> must list at least one entry"),
        "error must lead with the call-site flag help, got: {err}"
    );
}

#[test]
fn parse_csv_list_empty_string_is_error() {
    assert!(parse_csv_list(Some(""), "--flag=<x>").is_err());
}

// -----------------------------------------------------------------------
// detect_duplicate_paths
// -----------------------------------------------------------------------

#[test]
fn detect_duplicate_paths_unique_is_ok() {
    let a = Path::new("dist/a.tar.gz");
    let b = Path::new("dist/b.tar.gz");
    assert!(detect_duplicate_paths([a, b]).is_ok());
}

#[test]
fn detect_duplicate_paths_flags_repeat_with_count() {
    let a = Path::new("dist/a.tar.gz");
    let err = detect_duplicate_paths([a, a, a]).unwrap_err().to_string();
    assert!(
        err.contains("dist/a.tar.gz (3×)"),
        "must name the duplicated path with its occurrence count, got: {err}"
    );
}

#[test]
fn detect_duplicate_paths_empty_iter_is_ok() {
    let none: [&Path; 0] = [];
    assert!(detect_duplicate_paths(none).is_ok());
}

// -----------------------------------------------------------------------
// detect_missing_files
// -----------------------------------------------------------------------

#[test]
fn detect_missing_files_present_relative_under_dist_is_ok() {
    let dist = tempfile::tempdir().unwrap();
    std::fs::write(dist.path().join("a.bin"), b"x").unwrap();
    let rel = Path::new("a.bin");
    assert!(detect_missing_files([rel], dist.path()).is_ok());
}

#[test]
fn detect_missing_files_absent_bails_naming_dist_root() {
    let dist = tempfile::tempdir().unwrap();
    let rel = Path::new("ghost.bin");
    let err = detect_missing_files([rel], dist.path())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("ghost.bin"),
        "missing file must be named in the diagnostic, got: {err}"
    );
    assert!(
        err.contains(&dist.path().display().to_string()),
        "diagnostic must name the dist root it searched, got: {err}"
    );
}

#[test]
fn detect_missing_files_absolute_path_checked_literally() {
    let dist = tempfile::tempdir().unwrap();
    let abs = dist.path().join("nope.bin");
    let err = detect_missing_files([abs.as_path()], dist.path())
        .unwrap_err()
        .to_string();
    assert!(err.contains("nope.bin"));
}

// -----------------------------------------------------------------------
// referenced_var_keys
// -----------------------------------------------------------------------

#[test]
fn referenced_var_keys_lifts_each_var_dot_identifier() {
    let keys = referenced_var_keys("{{ .Var.FOO }}-{{ .Var.bar_baz }}");
    assert_eq!(keys, vec!["FOO", "bar_baz"]);
}

#[test]
fn referenced_var_keys_stops_at_non_identifier_char() {
    // The identifier ends at the dot / brace, not at end-of-string.
    let keys = referenced_var_keys("Var.alpha.Var.beta}");
    assert_eq!(keys, vec!["alpha", "beta"]);
}

#[test]
fn referenced_var_keys_empty_when_no_var_prefix() {
    assert!(referenced_var_keys("{{ .Version }} no vars here").is_empty());
}

#[test]
fn referenced_var_keys_bare_var_dot_with_no_identifier_yields_nothing() {
    // `Var.` immediately followed by a non-ident char contributes no key.
    assert!(referenced_var_keys("Var. ").is_empty());
}

// -----------------------------------------------------------------------
// yaml_key_sort_key
// -----------------------------------------------------------------------

#[test]
fn yaml_key_sort_key_strings_compare_on_raw_value() {
    let v = serde_yaml_ng::Value::String("zeta".to_string());
    assert_eq!(yaml_key_sort_key(&v), "zeta");
}

#[test]
fn yaml_key_sort_key_non_string_falls_back_to_debug() {
    let v = serde_yaml_ng::Value::Number(7.into());
    // A non-string key must still produce a deterministic, non-empty key.
    assert_eq!(yaml_key_sort_key(&v), format!("{:?}", v));
}

// -----------------------------------------------------------------------
// resolve_force_token_with_env (injected EnvSource — no process-env mutation)
// -----------------------------------------------------------------------

#[test]
fn resolve_force_token_config_field_wins_over_env() {
    let config = Config {
        force_token: Some(ForceTokenKind::Gitea),
        ..Default::default()
    };
    let env = anodizer_core::MapEnvSource::new().with("ANODIZER_FORCE_TOKEN", "gitlab");
    assert_eq!(
        resolve_force_token_with_env(&config, &env),
        Some(ForceTokenKind::Gitea)
    );
}

#[test]
fn resolve_force_token_reads_anodizer_env_case_insensitively() {
    let config = Config::default();
    let env = anodizer_core::MapEnvSource::new().with("ANODIZER_FORCE_TOKEN", "GitLab");
    assert_eq!(
        resolve_force_token_with_env(&config, &env),
        Some(ForceTokenKind::GitLab)
    );
}

#[test]
fn resolve_force_token_goreleaser_alias_is_fallback() {
    let config = Config::default();
    let env = anodizer_core::MapEnvSource::new().with("GORELEASER_FORCE_TOKEN", "github");
    assert_eq!(
        resolve_force_token_with_env(&config, &env),
        Some(ForceTokenKind::GitHub)
    );
}

#[test]
fn resolve_force_token_anodizer_var_takes_precedence_over_goreleaser_alias() {
    let config = Config::default();
    let env = anodizer_core::MapEnvSource::new()
        .with("ANODIZER_FORCE_TOKEN", "gitea")
        .with("GORELEASER_FORCE_TOKEN", "gitlab");
    assert_eq!(
        resolve_force_token_with_env(&config, &env),
        Some(ForceTokenKind::Gitea)
    );
}

#[test]
fn resolve_force_token_unrecognized_backend_is_none() {
    let config = Config::default();
    let env = anodizer_core::MapEnvSource::new().with("ANODIZER_FORCE_TOKEN", "bitbucket");
    assert_eq!(resolve_force_token_with_env(&config, &env), None);
}

#[test]
fn resolve_force_token_unset_everywhere_is_none() {
    let config = Config::default();
    let env = anodizer_core::MapEnvSource::new();
    assert_eq!(resolve_force_token_with_env(&config, &env), None);
}

fn quiet_log() -> StageLogger {
    StageLogger::new("test", anodizer_core::log::Verbosity::Quiet)
}

// ---- sort_yaml_mapping — Tagged-node recursion ---------------------

/// A `!Tag`-tagged YAML mapping must still have its inner keys sorted —
/// the `Value::Tagged` arm recurses into the wrapped value. Without that
/// arm a tagged sub-map would emit in source order and drift the
/// determinism fingerprint.
#[test]
fn sort_yaml_mapping_sorts_inside_tagged_node() {
    use serde_yaml_ng::value::{Tag, TaggedValue};
    use serde_yaml_ng::{Mapping, Value};
    let mut inner = Mapping::new();
    inner.insert(Value::from("z"), Value::from(1));
    inner.insert(Value::from("a"), Value::from(2));
    let mut value = Value::Tagged(Box::new(TaggedValue {
        tag: Tag::new("Custom"),
        value: Value::Mapping(inner),
    }));
    sort_yaml_mapping(&mut value);
    let out = serde_yaml_ng::to_string(&value).unwrap();
    let a_pos = out.find("a:").expect("a: present");
    let z_pos = out.find("z:").expect("z: present");
    assert!(
        a_pos < z_pos,
        "keys inside a tagged node must be sorted; got {out:?}"
    );
}

// ---- token-presence hard error in setup_env ------------------------

/// A crate carrying a `release:` block, no token, and a non-snapshot /
/// non-dry-run / non-publish-only run must bail with the GitHub-specific
/// hint (the default token_type). Gated + serial: `setup_env` may mutate
/// process env through the default token-file loader.
#[cfg(unix)]
#[test]
#[serial_test::serial(setup_env)]
fn setup_env_missing_token_bails_with_github_hint() {
    let config = Config {
        project_name: "p".to_string(),
        crates: vec![CrateConfig {
            name: "p".to_string(),
            release: Some(anodizer_core::config::ReleaseConfig::default()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = ctx_with_env(&config, &[]);
    let err = setup_env(&mut ctx, &config, &quiet_log())
        .expect_err("a release-configured run with no token must bail");
    assert!(
        err.to_string().contains("no GitHub token found"),
        "unexpected error: {err}"
    );
}

/// Snapshot mode must short-circuit the missing-token gate — a tokenless
/// snapshot is a supported local-validation flow.
#[cfg(unix)]
#[test]
#[serial_test::serial(setup_env)]
fn setup_env_missing_token_ok_in_snapshot() {
    let config = Config {
        project_name: "p".to_string(),
        crates: vec![CrateConfig {
            name: "p".to_string(),
            release: Some(anodizer_core::config::ReleaseConfig::default()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let opts = ContextOptions {
        snapshot: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), opts);
    ctx.set_env_source(anodizer_core::env_source::MapEnvSource::new());
    setup_env(&mut ctx, &config, &quiet_log())
        .expect("snapshot mode must skip the missing-token gate");
}

/// Two SCM tokens set without `force_token` is ambiguous — setup_env must
/// bail naming both offenders so the operator knows to set force_token.
/// Gated + serial: drives setup_env's process-env touchpoints.
#[cfg(unix)]
#[test]
#[serial_test::serial(setup_env)]
fn setup_env_multiple_tokens_without_force_bails() {
    let config = Config {
        project_name: "p".to_string(),
        ..Default::default()
    };
    let mut ctx = ctx_with_env(&config, &[("GITHUB_TOKEN", "gh"), ("GITLAB_TOKEN", "gl")]);
    let err = setup_env(&mut ctx, &config, &quiet_log())
        .expect_err("two tokens without force_token must bail");
    let msg = err.to_string();
    assert!(
        msg.contains("multiple SCM tokens set simultaneously")
            && msg.contains("GITHUB_TOKEN")
            && msg.contains("GITLAB_TOKEN"),
        "unexpected error: {msg}"
    );
}

// ---- write_metadata_and_artifacts — mod_timestamp application ------

/// `metadata.mod_timestamp` (when it renders non-empty) must be parsed and
/// stamped onto both metadata.json and artifacts.json. Assert the files
/// land and the mtime matches the parsed epoch — proving the stamp arm
/// (not just the write) ran.
#[test]
fn write_metadata_and_artifacts_applies_mod_timestamp() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        project_name: "demo".to_string(),
        dist: tmp.path().to_path_buf(),
        metadata: Some(anodizer_core::config::MetadataConfig {
            mod_timestamp: Some("1700000000".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ContextOptions::default());
    write_metadata_and_artifacts(&mut ctx, &config, &quiet_log())
        .expect("metadata + artifacts write must succeed");

    let meta = tmp.path().join("metadata.json");
    let arts = tmp.path().join("artifacts.json");
    assert!(meta.is_file(), "metadata.json must be written");
    assert!(arts.is_file(), "artifacts.json must be written");
    let mtime = std::fs::metadata(&meta)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(
        mtime, 1700000000,
        "metadata.json mtime must equal the rendered mod_timestamp epoch"
    );
}

/// metadata.json must register as a Metadata artifact so downstream stages
/// can pick it up; artifacts.json must NOT self-register.
#[test]
fn write_metadata_and_artifacts_registers_metadata_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        project_name: "demo".to_string(),
        dist: tmp.path().to_path_buf(),
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ContextOptions::default());
    write_metadata_and_artifacts(&mut ctx, &config, &quiet_log()).expect("write");
    let kinds: Vec<_> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| a.name == "metadata.json")
        .map(|a| a.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![ArtifactKind::Metadata],
        "exactly one metadata.json artifact of kind Metadata must be registered"
    );
}

// ---- write_metadata_json — release_url emission --------------------

/// metadata.json must carry the `ReleaseURL` the release stage resolved
/// into the template var (authoritative `html_url` or its derived
/// default). The action-side `release-url` output reads `.release_url`
/// from this file; announce/webhook templates render the same var, so
/// the two surfaces must agree byte-for-byte. Single-crate shape: one
/// crate, root dist.
#[test]
fn write_metadata_json_emits_release_url_from_context() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        project_name: "demo".to_string(),
        dist: tmp.path().to_path_buf(),
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ContextOptions::default());
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    ctx.set_release_url("https://github.com/acme/demo/releases/tag/v1.2.3");

    let path = write_metadata_json(&ctx, &config, &quiet_log()).expect("metadata write");
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        json["release_url"], "https://github.com/acme/demo/releases/tag/v1.2.3",
        "release_url must mirror the ReleaseURL template var"
    );
    assert_eq!(json["tag"], "v1.2.3");
}

/// When no release URL is derivable (snapshot with the release stage
/// skipped, `--skip=release`, no SCM repo configured) `ReleaseURL`
/// stays unset and `release_url` must emit as an empty string — the
/// same absent-value shape as the sibling `tag` / `previous_tag` /
/// `commit` keys, and `jq '.release_url // empty'` on the consumer
/// side still yields empty output.
#[test]
fn write_metadata_json_release_url_empty_when_unset() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config {
        project_name: "demo".to_string(),
        dist: tmp.path().to_path_buf(),
        ..Default::default()
    };
    let ctx = Context::new(config.clone(), ContextOptions::default());
    assert!(
        ctx.template_vars().get("ReleaseURL").is_none(),
        "precondition: ReleaseURL starts unset"
    );

    let path = write_metadata_json(&ctx, &config, &quiet_log()).expect("metadata write");
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        json["release_url"], "",
        "unset ReleaseURL must emit an empty release_url, matching the \
             empty-string style of the sibling keys"
    );
}

/// Workspace-lockstep shape: multiple crates, one shared version/tag,
/// ONE metadata.json at the workspace-root dist. The root file must
/// carry the release URL the pipeline resolved for the shared tag.
#[test]
fn write_metadata_json_lockstep_root_carries_shared_release_url() {
    let tmp = tempfile::tempdir().unwrap();
    let root_dist = tmp.path().join("dist");
    let config = Config {
        project_name: "cfgd".to_string(),
        dist: root_dist.clone(),
        crates: vec![
            anodizer_core::config::CrateConfig {
                name: "cfgd".to_string(),
                tag_template: Some("v{{ Version }}".to_string()),
                ..Default::default()
            },
            anodizer_core::config::CrateConfig {
                name: "cfgd-core".to_string(),
                tag_template: Some("v{{ Version }}".to_string()),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ContextOptions::default());
    ctx.template_vars_mut().set("Version", "0.4.0");
    ctx.template_vars_mut().set("Tag", "v0.4.0");
    ctx.set_release_url("https://github.com/acme/cfgd/releases/tag/v0.4.0");

    let path = write_metadata_json(&ctx, &config, &quiet_log()).expect("metadata write");
    assert_eq!(
        path,
        root_dist.join("metadata.json"),
        "lockstep writes a single metadata.json at the workspace-root dist"
    );
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        json["release_url"], "https://github.com/acme/cfgd/releases/tag/v0.4.0",
        "root metadata must carry the shared-tag release URL"
    );
    assert_eq!(json["tag"], "v0.4.0");
}

// ---- load_artifacts_from_manifest — targetless dedup skip ----------

/// Two manifest entries sharing the same path with `target: null` (e.g. a
/// source archive duplicated across shard manifests) must collapse to a
/// single registry entry — the second is skipped, not re-added.
#[test]
fn load_manifest_dedupes_targetless_duplicate_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let dist = tmp.path();
    let manifest = dist.join("artifacts.json");
    std::fs::write(
            &manifest,
            r#"[
              {"kind":"archive","name":"src.tar.gz","path":"dist/src.tar.gz","target":null,"crate_name":"demo","metadata":{},"size":null},
              {"kind":"archive","name":"src.tar.gz","path":"dist/src.tar.gz","target":null,"crate_name":"demo","metadata":{},"size":null}
            ]"#,
        )
        .unwrap();
    let config = Config {
        dist: dist.to_path_buf(),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    load_artifacts_from_manifest(&mut ctx, dist, &manifest).expect("load manifest");
    let count = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| a.path == dist.join("src.tar.gz"))
        .count();
    assert_eq!(
        count, 1,
        "a targetless artifact duplicated across shard manifests must register once"
    );
}

// ---- publish-only rehydrate → ChecksumStage idempotence ------------
//
// Exercises the exact runtime sequence `publish_only::run_one_crate_dist`
// performs — REAL `load_artifacts_from_manifest` (rehydrate a preserved
// dist whose manifest already records the per-target Checksum sidecars
// with a recorded size) followed by a REAL `ChecksumStage.run` in split
// mode (which re-emits those same `<archive>.sha256` sidecars). The
// determinism harness never runs the publish-side stages, and the
// builders-level tests only assert stage ORDER, so this is the only place
// the rehydrate→re-checksum slice executes. A non-idempotent
// `ArtifactRegistry::add` re-appends each sidecar at its already-present
// (path, Checksum) coordinate, doubling it for every downstream publisher.

/// A re-checksum over a rehydrated registry that already holds the
/// per-target Checksum sidecars must leave exactly one artifact per
/// (path, kind) — the re-add at an existing coordinate is an idempotent
/// update, never a second entry.
#[test]
fn rehydrate_then_checksum_split_has_no_duplicate_artifacts() {
    use anodizer_core::config::{ChecksumConfig, CrateConfig};
    use anodizer_core::stage::Stage;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_stage_checksum::ChecksumStage;

    let tmp = tempfile::TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    // A prior shard already produced the archive AND its split sidecar on
    // disk; the preserved dist carries both.
    let archive = dist.join("myapp-1.0.0-linux-amd64.tar.gz");
    std::fs::write(&archive, b"fake archive content").unwrap();
    let sidecar = dist.join("myapp-1.0.0-linux-amd64.tar.gz.sha256");
    std::fs::write(&sidecar, b"0".repeat(64)).unwrap();

    // The preserved manifest records the archive and its Checksum sidecar
    // with a concrete `size`, exactly as a post-pipeline `artifacts.json`
    // would after a split-mode shard run.
    let manifest = dist.join("artifacts.json");
    std::fs::write(
            &manifest,
            r#"[
              {"kind":"archive","name":"myapp-1.0.0-linux-amd64.tar.gz","path":"dist/myapp-1.0.0-linux-amd64.tar.gz","target":"x86_64-unknown-linux-gnu","crate_name":"myapp","metadata":{},"size":20},
              {"kind":"checksum","name":"myapp-1.0.0-linux-amd64.tar.gz.sha256","path":"dist/myapp-1.0.0-linux-amd64.tar.gz.sha256","target":"x86_64-unknown-linux-gnu","crate_name":"myapp","metadata":{"algorithm":"sha256"},"size":64}
            ]"#,
        )
        .unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            checksum: Some(ChecksumConfig {
                split: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    // Rehydrate via the same loader publish-only uses.
    load_artifacts_from_manifest(&mut ctx, &dist, &manifest).expect("rehydrate manifest");
    let rehydrated = ctx.artifacts.all().len();
    assert_eq!(rehydrated, 2, "manifest seeds the archive + its sidecar");

    // Re-run the checksum stage over the rehydrated registry — split mode
    // re-emits the same `<archive>.sha256` sidecar.
    ChecksumStage.run(&mut ctx).expect("checksum stage");

    // No (path, kind) pair may appear twice.
    let mut seen: std::collections::HashSet<(PathBuf, ArtifactKind)> =
        std::collections::HashSet::new();
    for a in ctx.artifacts.all() {
        assert!(
            seen.insert((a.path.clone(), a.kind)),
            "duplicate (path, kind) after re-checksum: {} / {:?}",
            a.path.display(),
            a.kind
        );
    }

    // Re-checksum is idempotent: the registry holds exactly the rehydrated
    // unique set, not a doubled one.
    assert_eq!(
        ctx.artifacts.all().len(),
        rehydrated,
        "split re-checksum over a rehydrated dist must not add a duplicate sidecar"
    );
}

// ---- publish-only rehydrate → AttestStage emit idempotence ---------
//
// The emit-mode in-toto statement registers as `UploadableFile`, a kind
// `ArtifactRegistry::add` deliberately does NOT collapse (a same-path
// UploadableFile collision is a real emission bug for genuine user assets).
// A preserved dist produced by an emit-mode harness run carries that
// statement in its manifest, so a publish-only re-run rehydrates it and
// then AttestStage re-emits it byte-for-byte at the same path. Without an
// already-present guard the re-add duplicates `(path, UploadableFile)`,
// and every downstream publisher re-processes the doubled asset.

/// AttestStage emit mode re-run over a rehydrated dist that already holds
/// the in-toto statement must leave exactly one artifact per (path, kind)
/// — the re-emit at an existing UploadableFile coordinate is a skip, never
/// a second entry.
#[test]
fn rehydrate_then_attest_emit_has_no_duplicate_artifacts() {
    use anodizer_core::config::{AttestationConfig, AttestationMode, CrateConfig};
    use anodizer_core::stage::Stage;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_stage_attest::AttestStage;

    let tmp = tempfile::TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    // A prior emit-mode shard produced the archive AND the in-toto
    // statement on disk; the preserved dist carries both.
    let archive = dist.join("myapp-1.0.0-linux-amd64.tar.gz");
    std::fs::write(&archive, b"fake archive content").unwrap();
    let statement = dist.join(AttestationConfig::STATEMENT_NAME);
    std::fs::write(
        &statement,
        b"{\"_type\":\"https://in-toto.io/Statement/v1\"}\n",
    )
    .unwrap();

    // The preserved manifest records the archive (with its sha256, the
    // subject digest attestation reuses) and the emit statement as an
    // UploadableFile, exactly as a post-pipeline `artifacts.json` would
    // after an emit-mode shard run.
    let manifest = dist.join("artifacts.json");
    std::fs::write(
            &manifest,
            r#"[
              {"kind":"archive","name":"myapp-1.0.0-linux-amd64.tar.gz","path":"dist/myapp-1.0.0-linux-amd64.tar.gz","target":"x86_64-unknown-linux-gnu","crate_name":"myapp","metadata":{"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"size":20},
              {"kind":"uploadable_file","name":"attestation.intoto.jsonl","path":"dist/attestation.intoto.jsonl","crate_name":"myapp","metadata":{"attestation_statement":"true"},"size":42}
            ]"#,
        )
        .unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }])
        .build();
    ctx.config.attestations = Some(AttestationConfig {
        enabled: true,
        mode: Some(AttestationMode::Emit),
        ..Default::default()
    });

    // Rehydrate via the same loader publish-only uses.
    load_artifacts_from_manifest(&mut ctx, &dist, &manifest).expect("rehydrate manifest");
    let rehydrated = ctx.artifacts.all().len();
    assert_eq!(rehydrated, 2, "manifest seeds the archive + its statement");

    // Re-run the attest stage over the rehydrated registry — emit mode
    // re-derives the same statement at the same path.
    AttestStage.run(&mut ctx).expect("attest stage");

    // No (path, kind) pair may appear twice across ALL kinds.
    let mut seen: std::collections::HashSet<(PathBuf, ArtifactKind)> =
        std::collections::HashSet::new();
    for a in ctx.artifacts.all() {
        assert!(
            seen.insert((a.path.clone(), a.kind)),
            "duplicate (path, kind) after re-emit: {} / {:?}",
            a.path.display(),
            a.kind
        );
    }

    // Re-emit is idempotent: the registry holds exactly the rehydrated
    // unique set, not a doubled one.
    assert_eq!(
        ctx.artifacts.all().len(),
        rehydrated,
        "emit re-run over a rehydrated dist must not add a duplicate statement"
    );
}

// ---- resolve_git_context — workspace fallback + snapshot defaults --
//
// resolve_git_context shells to `git` in the process cwd. Driving its
// crate-selection + snapshot-default branches hermetically needs the cwd
// swapped to an empty git repo with no tags. Gated (cwd swap is global)
// + serial.

#[cfg(unix)]
fn with_empty_git_repo_cwd(body: impl FnOnce()) {
    let tmp = tempfile::tempdir().unwrap();
    assert!(
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.args(["init", "-q"]).current_dir(tmp.path());
                cmd
            },
            "git",
        )
        .status
        .success(),
        "git init must succeed",
    );
    // The shared CwdGuard swaps into `tmp` and restores the original cwd on
    // Drop — panic-safe, so a panicking `body` still restores. Declared
    // after `tmp` so the guard drops (restores cwd) before the tempdir is
    // deleted.
    let _cwd = anodizer_core::test_helpers::CwdGuard::new(tmp.path()).unwrap();
    body();
}

/// Like [`with_empty_git_repo_cwd`] but seeds a committed, tagged HEAD and
/// then leaves the tree DIRTY (an uncommitted change), so the dirty-tree
/// guard in `resolve_git_context` is exercised against a real tag. Hermetic
/// committer identity is supplied via env so the helper never depends on a
/// global `git config`.
#[cfg(unix)]
fn with_tagged_dirty_repo_cwd(tag: &str, body: impl FnOnce()) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let git = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@e")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@e");
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} must succeed");
    };
    git(&["init", "-q"]);
    std::fs::write(dir.join("f.txt"), "v1\n").unwrap();
    git(&["add", "f.txt"]);
    git(&["commit", "-q", "-m", "init"]);
    git(&["tag", tag]);
    // Leave the tree dirty: an unstaged edit on the tagged commit.
    std::fs::write(dir.join("f.txt"), "v2\n").unwrap();

    // Swap into the tagged-dirty repo; CwdGuard restores on Drop. `dir`
    // borrows `tmp`, declared first, so it outlives the guard's restore.
    let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();
    body();
}

/// Like [`with_empty_git_repo_cwd`] but seeds two committed, tagged
/// commits — `older_tag` on the first commit, `head_tag` on HEAD — so a
/// latest-tag / previous-tag resolution has two same-family tags to
/// discover. Hermetic committer identity is supplied via env so the
/// helper never depends on a global `git config`.
#[cfg(unix)]
fn with_two_tagged_commits_repo_cwd(older_tag: &str, head_tag: &str, body: impl FnOnce()) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let git = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.args(args)
                    .current_dir(dir)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@e")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@e");
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} must succeed");
    };
    git(&["init", "-q"]);
    std::fs::write(dir.join("f.txt"), "v1\n").unwrap();
    git(&["add", "f.txt"]);
    git(&["commit", "-q", "-m", "init"]);
    git(&["tag", older_tag]);
    std::fs::write(dir.join("f.txt"), "v2\n").unwrap();
    git(&["add", "f.txt"]);
    git(&["commit", "-q", "-m", "second"]);
    git(&["tag", head_tag]);

    // The shared CwdGuard swaps into `tmp` and restores cwd on Drop
    // (panic-safe). Declared after `tmp` so it outlives the guard's
    // restore before the tempdir is deleted.
    let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();
    body();
}

/// Build a context backed by an EMPTY env source so the tag-discovery env
/// chain (`ANODIZER_CURRENT_TAG`, `GITHUB_REF_TYPE`/`GITHUB_REF_NAME`, …)
/// resolves to nothing. anodizer's own CI runs under GitHub Actions, which
/// exports `GITHUB_REF_*`; without this isolation those would leak in as a
/// tag override and mask the no-tag branches under test.
#[cfg(unix)]
fn empty_env_ctx(config: &Config, opts: ContextOptions) -> Context {
    let mut ctx = Context::new(config.clone(), opts);
    ctx.set_env_source(anodizer_core::env_source::MapEnvSource::new());
    ctx
}

/// A workspace-only config (no top-level crates) in snapshot mode must
/// resolve `first_crate` from the workspace fallback and, finding no tag,
/// default Version to 0.0.0 — proving both the workspace-crate selection
/// arm and the snapshot tag-default arm.
#[cfg(unix)]
#[test]
#[serial_test::serial(cwd)]
fn resolve_git_context_workspace_only_snapshot_defaults_version() {
    with_empty_git_repo_cwd(|| {
        let config = Config {
            project_name: "ws".to_string(),
            workspaces: Some(vec![WorkspaceConfig {
                name: "w".to_string(),
                crates: vec![CrateConfig {
                    name: "wcrate".to_string(),
                    path: ".".to_string(),
                    tag_template: Some("wcrate-v{{ .Version }}".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let opts = ContextOptions {
            snapshot: true,
            ..Default::default()
        };
        let mut ctx = empty_env_ctx(&config, opts);
        resolve_git_context(&mut ctx, &config, &quiet_log())
            .expect("snapshot workspace-only resolve must succeed");
        assert_eq!(
            ctx.template_vars().get("Version").map(String::as_str),
            Some("0.0.0"),
            "workspace-only snapshot must default Version to 0.0.0 via the v0.0.0 tag"
        );
    });
}

/// No crates and no workspaces: `first_crate` is None, so resolve_git_context
/// takes the bare `populate_git_vars` branch and returns Ok without touching
/// tag discovery. The `Tag` var stays unset (never populated from a crate).
#[cfg(unix)]
#[test]
#[serial_test::serial(cwd)]
fn resolve_git_context_no_crates_populates_vars_and_ok() {
    with_empty_git_repo_cwd(|| {
        let config = Config {
            project_name: "empty".to_string(),
            ..Default::default()
        };
        let mut ctx = empty_env_ctx(&config, ContextOptions::default());
        resolve_git_context(&mut ctx, &config, &quiet_log())
            .expect("no-crate config must resolve cleanly");
        assert!(
            ctx.template_vars().get("Tag").is_none(),
            "no crate means no tag-derived Tag var"
        );
    });
}

/// Non-snapshot, non-dry-run, no tags, with a selectable crate must be a
/// hard error: `resolve_git_context` bails demanding a tag or --snapshot.
#[cfg(unix)]
#[test]
#[serial_test::serial(cwd)]
fn resolve_git_context_no_tag_non_snapshot_bails() {
    with_empty_git_repo_cwd(|| {
        let config = Config {
            project_name: "x".to_string(),
            crates: vec![CrateConfig {
                name: "x".to_string(),
                path: ".".to_string(),
                tag_template: Some("x-v{{ .Version }}".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = empty_env_ctx(&config, ContextOptions::default());
        let err = resolve_git_context(&mut ctx, &config, &quiet_log())
            .expect_err("no tag + non-snapshot must bail");
        assert!(
            err.to_string().contains("no git tag found"),
            "unexpected error: {err}"
        );
    });
}

/// A crate with an UNSET `tag_template` in a `{name}-v`-convention
/// workspace: the latest-tag matcher and the previous-tag prefix filter
/// must resolve the SAME family. If the matcher instead falls back to
/// the built-in `v{{ Version }}` default (bucket A) it finds nothing
/// among the `widget-v*` tags and `resolve_git_context` bails demanding
/// a tag even though `widget-v0.2.0` sits right at HEAD.
#[cfg(unix)]
#[test]
#[serial_test::serial(cwd)]
fn resolve_git_context_unset_template_resolves_name_v_family_consistently() {
    with_two_tagged_commits_repo_cwd("widget-v0.1.0", "widget-v0.2.0", || {
        let config = Config {
            project_name: "widget".to_string(),
            crates: vec![CrateConfig {
                name: "widget".to_string(),
                path: ".".to_string(),
                tag_template: None,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = empty_env_ctx(&config, ContextOptions::default());
        resolve_git_context(&mut ctx, &config, &quiet_log()).expect(
            "an UNSET tag_template must still resolve the {name}-v convention \
                 family at HEAD, not bail for a missing bare-v tag",
        );
        assert_eq!(
            ctx.template_vars().get("Tag").map(String::as_str),
            Some("widget-v0.2.0"),
            "latest-tag matcher must find the {{name}}-v family tag at HEAD"
        );
        assert_eq!(
            ctx.git_info.as_ref().and_then(|gi| gi.previous_tag.clone()),
            Some("widget-v0.1.0".to_string()),
            "PreviousTag must resolve the SAME {{name}}-v family as the current tag"
        );
    });
}

/// The same no-tag, non-snapshot setup that bails above must NOT bail under
/// `notify: true`: a notification side-channel (e.g. an `on_error:` hook)
/// must render and send even with no tag, falling back to the v0.0.0
/// synthetic so `{{ Tag }}` still resolves.
#[cfg(unix)]
#[test]
#[serial_test::serial(cwd)]
fn resolve_git_context_notify_no_tag_defaults_v0() {
    with_empty_git_repo_cwd(|| {
        let config = Config {
            project_name: "x".to_string(),
            crates: vec![CrateConfig {
                name: "x".to_string(),
                path: ".".to_string(),
                tag_template: Some("x-v{{ .Version }}".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let opts = ContextOptions {
            notify: true,
            ..Default::default()
        };
        let mut ctx = empty_env_ctx(&config, opts);
        resolve_git_context(&mut ctx, &config, &quiet_log())
            .expect("notify must not bail on a missing tag");
        assert_eq!(
            ctx.template_vars().get("Version").map(String::as_str),
            Some("0.0.0"),
            "notify with no tag must default Version to 0.0.0"
        );
    });
}

/// A dirty working tree on a tagged HEAD is the exact state an `on_error:`
/// notify hook runs in after a failed release (partial `dist/`, in-flight
/// writeback). Without `notify` it is a hard bail; with `notify: true` it
/// must resolve cleanly so the alert is never lost.
#[cfg(unix)]
#[test]
#[serial_test::serial(cwd)]
fn resolve_git_context_notify_dirty_tree_does_not_bail() {
    with_tagged_dirty_repo_cwd("x-v0.1.0", || {
        let config = Config {
            project_name: "x".to_string(),
            crates: vec![CrateConfig {
                name: "x".to_string(),
                path: ".".to_string(),
                tag_template: Some("x-v{{ .Version }}".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };

        // Baseline: a dirty tree with default options is a hard bail.
        let mut bail_ctx = empty_env_ctx(&config, ContextOptions::default());
        let err = resolve_git_context(&mut bail_ctx, &config, &quiet_log())
            .expect_err("dirty tree + default options must bail");
        assert!(
            err.to_string().contains("dirty state"),
            "unexpected error: {err}"
        );

        // notify relaxes it: same dirty tree resolves cleanly.
        let opts = ContextOptions {
            notify: true,
            ..Default::default()
        };
        let mut ctx = empty_env_ctx(&config, opts);
        resolve_git_context(&mut ctx, &config, &quiet_log())
            .expect("notify must not bail on a dirty tree");
    });
}

// ---- auto_detect_github — no-remote warn path ----------------------

/// In a git repo with no `origin` remote, `auto_detect_github` can't detect
/// a repo, so a crate with a release block but no `github:` is left as-is
/// (the warn arm fires, no github filled). Gated + serial: cwd swap.
#[cfg(unix)]
#[test]
#[serial_test::serial(cwd)]
fn auto_detect_github_leaves_github_none_without_remote() {
    with_empty_git_repo_cwd(|| {
        let mut config = Config {
            project_name: "x".to_string(),
            crates: vec![CrateConfig {
                name: "x".to_string(),
                release: Some(anodizer_core::config::ReleaseConfig::default()),
                ..Default::default()
            }],
            ..Default::default()
        };
        auto_detect_github(&mut config, &quiet_log());
        assert!(
            config.crates[0].release.as_ref().unwrap().github.is_none(),
            "with no detectable remote, the missing github block must stay None"
        );
    });
}

/// The auto-detected slug fills a WORKSPACE crate's missing `github`
/// block, not just top-level entries — a workspace-only crate's release
/// stage reads the same per-crate override.
#[cfg(unix)]
#[test]
#[serial_test::serial(cwd)]
fn auto_detect_github_fills_workspace_crates_from_remote() {
    with_empty_git_repo_cwd(|| {
        assert!(
            std::process::Command::new("git")
                .args([
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/acme/widget.git"
                ])
                .status()
                .unwrap()
                .success(),
            "git remote add must succeed"
        );
        let mut config = Config {
            project_name: "x".to_string(),
            crates: vec![CrateConfig {
                name: "top".to_string(),
                release: Some(anodizer_core::config::ReleaseConfig::default()),
                ..Default::default()
            }],
            workspaces: Some(vec![WorkspaceConfig {
                name: "ws".to_string(),
                crates: vec![CrateConfig {
                    name: "member".to_string(),
                    release: Some(anodizer_core::config::ReleaseConfig::default()),
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            ..Default::default()
        };
        auto_detect_github(&mut config, &quiet_log());
        let top = config.crates[0].release.as_ref().unwrap();
        let top_gh = top
            .github
            .as_ref()
            .expect("top-level crate's github block must be filled");
        assert_eq!(
            (top_gh.owner.as_str(), top_gh.name.as_str()),
            ("acme", "widget")
        );
        let member = config.workspaces.as_ref().unwrap()[0].crates[0]
            .release
            .as_ref()
            .unwrap();
        let member_gh = member
            .github
            .as_ref()
            .expect("workspace crate's github block must be filled");
        assert_eq!(
            (member_gh.owner.as_str(), member_gh.name.as_str()),
            ("acme", "widget")
        );
    });
}

// ---- discover_workspace_root — override ancestor walk --------------

/// With a `--config` override pointing at a file inside a dir that has a
/// `Cargo.toml`, discovery walks up from the config's parent and returns
/// that dir (absolutized). Gated: asserts on an absolute unix path.
#[cfg(unix)]
#[test]
fn discover_workspace_root_override_finds_cargo_toml_ancestor() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
    let cfg = root.join(".anodizer.yaml");
    std::fs::write(&cfg, "project_name: x\n").unwrap();
    let found = discover_workspace_root(Some(&cfg)).expect("must find Cargo.toml ancestor");
    assert_eq!(
        found, root,
        "override discovery must return the absolute dir holding Cargo.toml"
    );
}

// ---- workspace scoping: resolve / infer / validate ------------------

fn scope_crate(name: &str) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some(format!("{}-v{{{{ .Version }}}}", name)),
        ..Default::default()
    }
}

fn scope_mixed_config() -> Config {
    use anodizer_core::config::WorkspaceConfig;
    Config {
        project_name: "test".to_string(),
        crates: vec![scope_crate("top")],
        workspaces: Some(vec![
            WorkspaceConfig {
                name: "ws-a".to_string(),
                crates: vec![scope_crate("a-one"), scope_crate("a-two")],
                ..Default::default()
            },
            WorkspaceConfig {
                name: "ws-b".to_string(),
                crates: vec![scope_crate("b-one")],
                ..Default::default()
            },
        ]),
        ..Default::default()
    }
}

#[test]
fn resolve_workspace_found() {
    let config = scope_mixed_config();
    let ws = resolve_workspace(&config, "ws-b").unwrap();
    assert_eq!(ws.name, "ws-b");
    assert_eq!(ws.crates.len(), 1);
    assert_eq!(ws.crates[0].name, "b-one");
}

#[test]
fn resolve_workspace_not_found_lists_available() {
    let config = scope_mixed_config();
    let msg = resolve_workspace(&config, "nonexistent")
        .unwrap_err()
        .to_string();
    assert!(msg.contains("nonexistent"), "names the missing ws: {msg}");
    assert!(
        msg.contains("ws-a") && msg.contains("ws-b"),
        "lists available workspaces: {msg}"
    );
}

#[test]
fn resolve_workspace_no_workspaces_defined() {
    let config = Config {
        project_name: "test".to_string(),
        ..Default::default()
    };
    let msg = resolve_workspace(&config, "anything")
        .unwrap_err()
        .to_string();
    assert!(msg.contains("no workspaces defined"), "got: {msg}");
}

#[test]
fn infer_workspace_single_workspace_selection_infers() {
    let config = scope_mixed_config();
    let inferred =
        infer_workspace_for_selection(&config, &["a-one".to_string(), "a-two".to_string()])
            .expect("single-workspace selection must not error");
    assert_eq!(inferred.as_deref(), Some("ws-a"));
}

#[test]
fn infer_workspace_top_level_only_selection_is_untouched() {
    let config = scope_mixed_config();
    let inferred = infer_workspace_for_selection(&config, &["top".to_string()])
        .expect("top-level selection must not error");
    assert_eq!(inferred, None);
}

#[test]
fn infer_workspace_mixed_selection_errors_in_both_orderings() {
    let config = scope_mixed_config();
    // Both orderings must yield the SAME hard error: the decision comes
    // from the whole selection set, never from whichever name is first.
    let forward = infer_workspace_for_selection(&config, &["a-one".to_string(), "top".to_string()])
        .expect_err("workspace + top-level selection must error");
    let reversed =
        infer_workspace_for_selection(&config, &["top".to_string(), "a-one".to_string()])
            .expect_err("reversed ordering must error identically");
    for err in [&forward, &reversed] {
        let msg = err.to_string();
        assert!(
            msg.contains("'a-one' (workspace 'ws-a')") && msg.contains("'top' (top-level)"),
            "error must name each crate and its home; got: {msg}"
        );
    }
}

#[test]
fn infer_workspace_two_workspace_selection_errors() {
    let config = scope_mixed_config();
    let err = infer_workspace_for_selection(&config, &["a-one".to_string(), "b-one".to_string()])
        .expect_err("selection spanning two workspaces must error");
    let msg = err.to_string();
    assert!(
        msg.contains("workspace 'ws-a'") && msg.contains("workspace 'ws-b'"),
        "error must name both workspaces; got: {msg}"
    );
}

#[test]
fn validate_selection_rejects_unknown_names() {
    let config = scope_mixed_config();
    let err = validate_selection_against_universe(&config, &["nope".to_string()], None)
        .expect_err("an unknown --crate name must be a hard error, not a silent drop");
    assert!(err.to_string().contains("nope"), "got: {err}");
    // Known names across the whole universe pass.
    validate_selection_against_universe(&config, &["top".to_string(), "b-one".to_string()], None)
        .expect("known names must validate");
}

#[test]
fn validate_selection_empty_universe_names_the_remediation() {
    let config = Config {
        project_name: "solo".to_string(),
        ..Default::default()
    };
    let err = validate_selection_against_universe(&config, &["alpha".to_string()], None)
        .expect_err("an empty crate universe must reject any --crate name");
    assert_eq!(
        err.to_string(),
        "--crate alpha: the configuration defines no crates; drop --crate to run at the \
             repo level, or add a `crates:` entry for 'alpha'"
    );
}

#[test]
fn merge_skip_stages_appends_only_missing_names() {
    let mut skips = vec!["publish".to_string()];
    merge_skip_stages(&mut skips, &["publish", "announce"]);
    merge_skip_stages(&mut skips, &["announce".to_string(), "blob".to_string()]);
    assert_eq!(skips, ["publish", "announce", "blob"]);
}

#[test]
fn validate_selection_names_workspace_scope_after_overlay() {
    let mut config = scope_mixed_config();
    let ws = config.workspaces.as_ref().unwrap()[0].clone();
    apply_workspace_overlay(&mut config, &ws);
    // Post-overlay the universe is ws-a only: a top-level crate name is
    // out of scope and the error must say WHY (the workspace scoping).
    let err = validate_selection_against_universe(&config, &["top".to_string()], Some("ws-a"))
        .expect_err("a crate outside the overlaid workspace must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("ws-a") && msg.contains("top"),
        "error must name the workspace scope and the crate; got: {msg}"
    );
}

#[test]
fn apply_workspace_scope_infers_and_returns_skip() {
    use anodizer_core::config::WorkspaceConfig;
    let mut config = scope_mixed_config();
    config.workspaces.as_mut().unwrap()[0] = WorkspaceConfig {
        name: "ws-a".to_string(),
        crates: vec![scope_crate("a-one"), scope_crate("a-two")],
        skip: vec!["upx".to_string()],
        ..Default::default()
    };
    let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
    let skip = apply_workspace_scope(&mut config, None, &["a-one".to_string()], &log)
        .expect("ws-member selection must infer its workspace");
    assert_eq!(skip, vec!["upx".to_string()], "workspace skip returned");
    assert!(
        config.workspaces.is_none(),
        "overlay must clear sibling workspaces"
    );
    let names: Vec<&str> = config.crates.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["a-one", "a-two"], "universe is ws-a's crates");
}
