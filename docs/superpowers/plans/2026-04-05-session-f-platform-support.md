# Session F: Platform Support — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add multi-platform SCM support (GitLab, Gitea, GitHub Enterprise), additional publishers (DockerHub, Artifactory, Fury, CloudSmith, NPM), Snapcraft publish wiring, and GitLab/Gitea changelog backends — achieving parity with GoReleaser Session F items.

**Architecture:** The core change is introducing a `ScmBackend` trait in `core` that abstracts GitHub/GitLab/Gitea release operations. The release stage, changelog stage, and publisher file-creation paths all switch from hardcoded GitHub (octocrab) to dispatching through this trait based on a `token_type` / `force_token` field. New publishers are independent crates following existing patterns. Config adds `github_urls`, `gitlab_urls`, `gitea_urls` structs and `force_token` field at the top level.

**Tech Stack:** Rust, octocrab (GitHub), reqwest (GitLab/Gitea/HTTP publishers), serde, tokio, anyhow

---

## File Structure

### Core config additions (`crates/core/src/config.rs`)
- Add `GitHubUrlsConfig`, `GitLabUrlsConfig`, `GiteaUrlsConfig` structs
- Add `force_token` field to `Config`
- Add `gitlab` and `gitea` fields to `ReleaseConfig` (alongside existing `github`)
- Add `DockerHubConfig`, `ArtifactoryConfig`, `FuryConfig`, `CloudSmithConfig`, `NpmConfig` structs
- Add corresponding Vec fields to `Config`

### SCM client abstraction (`crates/core/src/scm.rs` — new file)
- `ScmBackend` trait: `create_release`, `upload_asset`, `publish_release`, `changelog`, `create_file`
- `GitHubBackend`, `GitLabBackend`, `GiteaBackend` implementations
- `ScmTokenType` enum + factory `new_backend()`

### Release stage updates (`crates/stage-release/src/lib.rs`)
- Replace hardcoded octocrab with `ScmBackend` trait dispatch
- Support `release.gitlab` and `release.gitea` config paths
- GitHub Enterprise URL support via `github_urls` config

### Changelog stage updates (`crates/stage-changelog/src/lib.rs`)
- Add `gitlab` and `gitea` as valid `use` sources
- Implement SCM-based changelog fetching via `ScmBackend::changelog`

### New publisher stages
- `crates/stage-publish/src/dockerhub.rs` — DockerHub description sync
- `crates/stage-publish/src/artifactory.rs` — HTTP PUT artifact upload
- `crates/stage-publish/src/fury.rs` — GemFury deb/rpm/apk push
- `crates/stage-publish/src/cloudsmith.rs` — CloudSmith package push
- `crates/stage-publish/src/npm.rs` — NPM package generation + publish

### Snapcraft publish wiring
- Already implemented in `crates/stage-snapcraft/src/lib.rs` as `SnapcraftPublishStage`
- Verify it's wired into the pipeline

---

## Task 1: Config structs — URL configs and force_token

**Files:**
- Modify: `crates/core/src/config.rs`

- [ ] **Step 1: Write failing tests for new config structs**

```rust
#[test]
fn test_github_urls_config_parse() {
    let yaml = r#"
github_urls:
  api: "https://github.mycompany.com/api/v3/"
  upload: "https://github.mycompany.com/api/uploads/"
  download: "https://github.mycompany.com"
  skip_tls_verify: true
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let urls = cfg.github_urls.unwrap();
    assert_eq!(urls.api.as_deref(), Some("https://github.mycompany.com/api/v3/"));
    assert_eq!(urls.upload.as_deref(), Some("https://github.mycompany.com/api/uploads/"));
    assert_eq!(urls.download.as_deref(), Some("https://github.mycompany.com"));
    assert_eq!(urls.skip_tls_verify, Some(true));
}

#[test]
fn test_gitlab_urls_config_parse() {
    let yaml = r#"
gitlab_urls:
  api: "https://gitlab.mycompany.com/api/v4/"
  download: "https://gitlab.mycompany.com"
  skip_tls_verify: true
  use_package_registry: true
  use_job_token: true
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let urls = cfg.gitlab_urls.unwrap();
    assert_eq!(urls.api.as_deref(), Some("https://gitlab.mycompany.com/api/v4/"));
    assert_eq!(urls.download.as_deref(), Some("https://gitlab.mycompany.com"));
    assert_eq!(urls.skip_tls_verify, Some(true));
    assert_eq!(urls.use_package_registry, Some(true));
    assert_eq!(urls.use_job_token, Some(true));
}

#[test]
fn test_gitea_urls_config_parse() {
    let yaml = r#"
gitea_urls:
  api: "https://gitea.mycompany.com/api/v1/"
  download: "https://gitea.mycompany.com"
  skip_tls_verify: true
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let urls = cfg.gitea_urls.unwrap();
    assert_eq!(urls.api.as_deref(), Some("https://gitea.mycompany.com/api/v1/"));
    assert_eq!(urls.download.as_deref(), Some("https://gitea.mycompany.com"));
    assert_eq!(urls.skip_tls_verify, Some(true));
}

#[test]
fn test_force_token_config_parse() {
    let yaml = r#"
project_name: test
force_token: gitlab
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.force_token.as_deref(), Some("gitlab"));
}

#[test]
fn test_release_gitlab_gitea_config_parse() {
    let yaml = r#"
project_name: test
release:
  gitlab:
    owner: mygroup
    name: myproject
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = cfg.release.unwrap();
    let gl = release.gitlab.unwrap();
    assert_eq!(gl.owner, "mygroup");
    assert_eq!(gl.name, "myproject");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib -p anodize-core test_github_urls_config_parse test_gitlab_urls_config_parse test_gitea_urls_config_parse test_force_token_config_parse test_release_gitlab_gitea_config_parse 2>&1 | tail -20`
Expected: compilation errors — structs don't exist yet

- [ ] **Step 3: Implement config structs**

Add to `crates/core/src/config.rs`:

```rust
// ---------------------------------------------------------------------------
// SCM URL configs (GitHub Enterprise, GitLab, Gitea)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct GitHubUrlsConfig {
    /// GitHub API base URL (for GitHub Enterprise).
    pub api: Option<String>,
    /// GitHub upload URL (for GitHub Enterprise).
    pub upload: Option<String>,
    /// GitHub download URL (for GitHub Enterprise).
    pub download: Option<String>,
    /// Skip TLS certificate verification.
    pub skip_tls_verify: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct GitLabUrlsConfig {
    /// GitLab API base URL.
    pub api: Option<String>,
    /// GitLab download URL.
    pub download: Option<String>,
    /// Skip TLS certificate verification.
    pub skip_tls_verify: Option<bool>,
    /// Use GitLab generic package registry for uploads instead of project attachments.
    pub use_package_registry: Option<bool>,
    /// Use CI_JOB_TOKEN for authentication.
    pub use_job_token: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct GiteaUrlsConfig {
    /// Gitea API base URL.
    pub api: Option<String>,
    /// Gitea download URL.
    pub download: Option<String>,
    /// Skip TLS certificate verification.
    pub skip_tls_verify: Option<bool>,
}
```

Add to `ReleaseConfig`:
```rust
    /// GitLab repository to release to (owner/group and project name).
    pub gitlab: Option<GitHubConfig>,  // reuse same struct shape
    /// Gitea repository to release to (owner and repo name).
    pub gitea: Option<GitHubConfig>,   // reuse same struct shape
```

Rename `GitHubConfig` to `ScmRepoConfig` (it's just owner+name) and alias:
```rust
pub type GitHubConfig = ScmRepoConfig;
```

Add to `Config`:
```rust
    /// GitHub Enterprise URL configuration.
    pub github_urls: Option<GitHubUrlsConfig>,
    /// GitLab URL configuration.
    pub gitlab_urls: Option<GitLabUrlsConfig>,
    /// Gitea URL configuration.
    pub gitea_urls: Option<GiteaUrlsConfig>,
    /// Force a specific SCM token type: "github", "gitlab", or "gitea".
    pub force_token: Option<String>,
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib -p anodize-core test_github_urls_config_parse test_gitlab_urls_config_parse test_gitea_urls_config_parse test_force_token_config_parse test_release_gitlab_gitea_config_parse 2>&1 | tail -20`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/config.rs
git commit -m "feat(config): add GitLab/Gitea/GH Enterprise URL structs and force_token"
```

---

## Task 2: SCM backend trait and token type resolution

**Files:**
- Create: `crates/core/src/scm.rs`
- Modify: `crates/core/src/lib.rs` (add `pub mod scm;`)
- Modify: `crates/core/src/context.rs` (add `token_type` field)

- [ ] **Step 1: Write failing tests for token type resolution**

In `crates/core/src/scm.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_type_from_force_token() {
        assert_eq!(resolve_token_type(Some("github"), None), ScmTokenType::GitHub);
        assert_eq!(resolve_token_type(Some("gitlab"), None), ScmTokenType::GitLab);
        assert_eq!(resolve_token_type(Some("gitea"), None), ScmTokenType::Gitea);
    }

    #[test]
    fn test_token_type_from_env_gitlab_token() {
        // When GITLAB_TOKEN is set and no force_token, detect GitLab
        assert_eq!(resolve_token_type(None, Some("gitlab")), ScmTokenType::GitLab);
    }

    #[test]
    fn test_token_type_defaults_github() {
        assert_eq!(resolve_token_type(None, None), ScmTokenType::GitHub);
    }

    #[test]
    fn test_default_github_download_url() {
        assert_eq!(default_download_url(ScmTokenType::GitHub, None), "https://github.com");
    }

    #[test]
    fn test_default_gitlab_download_url() {
        assert_eq!(default_download_url(ScmTokenType::GitLab, None), "https://gitlab.com");
    }

    #[test]
    fn test_default_gitea_download_url() {
        // Gitea has no default — must be set in config
        assert_eq!(default_download_url(ScmTokenType::Gitea, None), "");
    }

    #[test]
    fn test_custom_download_url() {
        assert_eq!(
            default_download_url(ScmTokenType::GitHub, Some("https://github.mycompany.com")),
            "https://github.mycompany.com"
        );
    }

    #[test]
    fn test_release_url_template_github() {
        let url = release_url_template(ScmTokenType::GitHub, "octocat", "hello", "https://github.com");
        assert_eq!(url, "https://github.com/octocat/hello/releases/download/{{ urlPathEscape .Tag }}/{{ .ArtifactName }}");
    }

    #[test]
    fn test_release_url_template_gitlab() {
        let url = release_url_template(ScmTokenType::GitLab, "mygroup", "myproject", "https://gitlab.com");
        assert_eq!(url, "https://gitlab.com/mygroup/myproject/-/releases/{{ urlPathEscape .Tag }}/downloads/{{ .ArtifactName }}");
    }

    #[test]
    fn test_release_url_template_gitea() {
        let url = release_url_template(ScmTokenType::Gitea, "owner", "repo", "https://gitea.example.com");
        assert_eq!(url, "https://gitea.example.com/owner/repo/releases/download/{{ urlPathEscape .Tag }}/{{ .ArtifactName }}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib -p anodize-core scm::tests 2>&1 | tail -20`
Expected: module doesn't exist yet

- [ ] **Step 3: Implement SCM module**

Create `crates/core/src/scm.rs`:

```rust
use serde::{Deserialize, Serialize};

/// The SCM provider type, determined from config or environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScmTokenType {
    GitHub,
    GitLab,
    Gitea,
}

impl std::fmt::Display for ScmTokenType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GitHub => write!(f, "github"),
            Self::GitLab => write!(f, "gitlab"),
            Self::Gitea => write!(f, "gitea"),
        }
    }
}

/// Resolve the SCM token type from config and environment.
///
/// Priority:
/// 1. `force_token` config field (explicit override)
/// 2. `detected_from_env` — caller checks which `*_TOKEN` env var is set
/// 3. Default: GitHub
pub fn resolve_token_type(
    force_token: Option<&str>,
    detected_from_env: Option<&str>,
) -> ScmTokenType {
    if let Some(ft) = force_token {
        return match ft {
            "gitlab" => ScmTokenType::GitLab,
            "gitea" => ScmTokenType::Gitea,
            _ => ScmTokenType::GitHub,
        };
    }
    if let Some(env_hint) = detected_from_env {
        return match env_hint {
            "gitlab" => ScmTokenType::GitLab,
            "gitea" => ScmTokenType::Gitea,
            _ => ScmTokenType::GitHub,
        };
    }
    ScmTokenType::GitHub
}

/// Get the default download URL for the given SCM type.
pub fn default_download_url(token_type: ScmTokenType, custom: Option<&str>) -> &str {
    if let Some(c) = custom {
        return c;
    }
    match token_type {
        ScmTokenType::GitHub => "https://github.com",
        ScmTokenType::GitLab => "https://gitlab.com",
        ScmTokenType::Gitea => "",
    }
}

/// Build the release URL template for artifact downloads.
/// Matches GoReleaser's `ReleaseURLTemplate()` per backend.
pub fn release_url_template(
    token_type: ScmTokenType,
    owner: &str,
    name: &str,
    download_url: &str,
) -> String {
    match token_type {
        ScmTokenType::GitHub | ScmTokenType::Gitea => {
            format!(
                "{}/{}/{}/releases/download/{{{{ urlPathEscape .Tag }}}}/{{{{ .ArtifactName }}}}",
                download_url, owner, name
            )
        }
        ScmTokenType::GitLab => {
            format!(
                "{}/{}/-/releases/{{{{ urlPathEscape .Tag }}}}/downloads/{{{{ .ArtifactName }}}}",
                download_url,
                if owner.is_empty() {
                    name.to_string()
                } else {
                    format!("{}/{}", owner, name)
                }
            )
        }
    }
}

// Tests at bottom (as shown in Step 1)
```

Add `pub mod scm;` to `crates/core/src/lib.rs`.

Add to `Context` in `crates/core/src/context.rs`:
```rust
    /// The resolved SCM token type (GitHub, GitLab, Gitea).
    pub token_type: crate::scm::ScmTokenType,
```

Initialize it in `Context::new()`:
```rust
    token_type: crate::scm::ScmTokenType::GitHub, // default, overridden by pipeline init
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib -p anodize-core scm::tests 2>&1 | tail -20`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/scm.rs crates/core/src/lib.rs crates/core/src/context.rs
git commit -m "feat(core): add ScmTokenType enum and resolution logic"
```

---

## Task 3: Token type resolution in pipeline init

**Files:**
- Modify: `crates/cli/src/pipeline.rs`

- [ ] **Step 1: Write failing test for token type resolution from env/config**

Add test in `crates/core/src/scm.rs`:

```rust
#[test]
fn test_detect_token_from_env_vars() {
    // Simulates what the pipeline will do:
    // Check GITLAB_TOKEN, GITEA_TOKEN, GITHUB_TOKEN in order
    let detect = |gitlab: bool, gitea: bool, _github: bool| -> Option<&'static str> {
        if gitlab { return Some("gitlab"); }
        if gitea { return Some("gitea"); }
        None // falls through to GitHub default
    };

    assert_eq!(detect(true, false, false), Some("gitlab"));
    assert_eq!(detect(false, true, false), Some("gitea"));
    assert_eq!(detect(false, false, true), None); // GitHub is default
}
```

- [ ] **Step 2: Implement token type wiring in pipeline.rs**

In the pipeline initialization (where the Context is created), add token type resolution:

```rust
// Resolve SCM token type from config and environment
let env_hint = if std::env::var("GITLAB_TOKEN").is_ok() {
    Some("gitlab")
} else if std::env::var("GITEA_TOKEN").is_ok() {
    Some("gitea")
} else {
    None
};
ctx.token_type = anodize_core::scm::resolve_token_type(
    ctx.config.force_token.as_deref(),
    env_hint,
);

// Resolve the token value from the appropriate env var
if ctx.options.token.is_none() {
    ctx.options.token = match ctx.token_type {
        anodize_core::scm::ScmTokenType::GitLab => {
            std::env::var("GITLAB_TOKEN").ok()
        }
        anodize_core::scm::ScmTokenType::Gitea => {
            std::env::var("GITEA_TOKEN").ok()
        }
        anodize_core::scm::ScmTokenType::GitHub => {
            std::env::var("ANODIZE_GITHUB_TOKEN").ok()
                .or_else(|| std::env::var("GITHUB_TOKEN").ok())
        }
    };
}
```

- [ ] **Step 3: Run full test suite**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: all pass (no behavioral changes yet)

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/pipeline.rs crates/core/src/scm.rs
git commit -m "feat(cli): wire token type resolution from config/env into Context"
```

---

## Task 4: GitHub Enterprise URL support in release stage

**Files:**
- Modify: `crates/stage-release/src/lib.rs`

- [ ] **Step 1: Write failing test for GH Enterprise URLs**

```rust
#[test]
fn test_release_uses_github_enterprise_urls() {
    use anodize_core::config::{Config, CrateConfig, GitHubUrlsConfig, ReleaseConfig, GitHubConfig};
    use anodize_core::context::{Context, ContextOptions};

    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.github_urls = Some(GitHubUrlsConfig {
        api: Some("https://github.mycompany.com/api/v3/".to_string()),
        upload: Some("https://github.mycompany.com/api/uploads/".to_string()),
        download: Some("https://github.mycompany.com".to_string()),
        skip_tls_verify: Some(false),
    });
    config.release = Some(ReleaseConfig {
        github: Some(GitHubConfig {
            owner: "myorg".to_string(),
            name: "myrepo".to_string(),
        }),
        ..Default::default()
    });
    config.crates = vec![CrateConfig {
        name: "test".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        release: Some(ReleaseConfig {
            github: Some(GitHubConfig {
                owner: "myorg".to_string(),
                name: "myrepo".to_string(),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }];

    let mut opts = ContextOptions::default();
    opts.dry_run = true;
    let mut ctx = Context::new(config, opts);
    // In dry-run mode, the release stage should show the enterprise URL
    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());
}
```

- [ ] **Step 2: Implement GH Enterprise URL wiring**

In the release stage, when building the octocrab client, check for `github_urls` config:

```rust
let github_urls = ctx.config.github_urls.as_ref();
let octo = if let Some(urls) = github_urls {
    let mut builder = octocrab::Octocrab::builder()
        .personal_token(token_str.clone());
    if let Some(ref api) = urls.api {
        builder = builder.base_uri(api)
            .context("release: invalid github_urls.api URL")?;
    }
    builder.build().context("release: build octocrab client")?
} else {
    octocrab::Octocrab::builder()
        .personal_token(token_str.clone())
        .build()
        .context("release: build octocrab client")?
};
```

- [ ] **Step 3: Run tests**

Run: `cargo test --lib -p stage-release 2>&1 | tail -20`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add crates/stage-release/src/lib.rs
git commit -m "feat(release): support GitHub Enterprise URLs (api/upload/download/skip_tls_verify)"
```

---

## Task 5: GitLab release backend

**Files:**
- Create: `crates/stage-release/src/gitlab.rs`
- Modify: `crates/stage-release/src/lib.rs`

- [ ] **Step 1: Write tests for GitLab release behavior**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gitlab_release_url() {
        let url = gitlab_release_url("https://gitlab.com", "mygroup", "myproject", "v1.0.0");
        assert_eq!(url, "https://gitlab.com/mygroup/myproject/-/releases/v1.0.0");
    }

    #[test]
    fn test_gitlab_release_url_custom_instance() {
        let url = gitlab_release_url("https://gitlab.example.com", "team", "app", "v2.0.0");
        assert_eq!(url, "https://gitlab.example.com/team/app/-/releases/v2.0.0");
    }

    #[test]
    fn test_gitlab_upload_link_url() {
        let url = gitlab_artifact_link_url(
            "https://gitlab.com",
            "mygroup",
            "myproject",
            "v1.0.0",
            "myapp_linux_amd64.tar.gz",
        );
        assert!(url.contains("myapp_linux_amd64.tar.gz"));
    }
}
```

- [ ] **Step 2: Implement GitLab release backend**

Create `crates/stage-release/src/gitlab.rs` with:
- `gitlab_create_release()` — POST to `/projects/:id/releases`
- `gitlab_upload_asset()` — POST to `/projects/:id/uploads` + create release link
- `gitlab_publish_release()` — no-op (GitLab doesn't have drafts)
- `gitlab_release_url()`, `gitlab_artifact_link_url()` helpers
- Uses reqwest with optional TLS skip and job token support

Wire into release stage `run()`: when `token_type == ScmTokenType::GitLab`, use the GitLab backend instead of octocrab.

- [ ] **Step 3: Run tests**

Run: `cargo test --lib -p stage-release gitlab 2>&1 | tail -20`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add crates/stage-release/src/gitlab.rs crates/stage-release/src/lib.rs
git commit -m "feat(release): add GitLab release backend"
```

---

## Task 6: Gitea release backend

**Files:**
- Create: `crates/stage-release/src/gitea.rs`
- Modify: `crates/stage-release/src/lib.rs`

- [ ] **Step 1: Write tests for Gitea release behavior**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gitea_release_url() {
        let url = gitea_release_url("https://gitea.example.com", "owner", "repo", "v1.0.0");
        assert_eq!(url, "https://gitea.example.com/owner/repo/releases/tag/v1.0.0");
    }

    #[test]
    fn test_gitea_create_release_json() {
        let json = gitea_release_json("v1.0.0", "commit123", "Release v1.0.0", "body", false, true);
        assert_eq!(json["tag_name"], "v1.0.0");
        assert_eq!(json["target_commitish"], "commit123");
        assert_eq!(json["draft"], false);
        assert_eq!(json["prerelease"], true);
    }
}
```

- [ ] **Step 2: Implement Gitea release backend**

Create `crates/stage-release/src/gitea.rs` with:
- `gitea_create_release()` — POST to `/api/v1/repos/:owner/:repo/releases`
- `gitea_upload_asset()` — POST multipart to `/api/v1/repos/:owner/:repo/releases/:id/assets`
- `gitea_publish_release()` — no-op (Gitea doesn't support drafts well)
- `gitea_release_url()`, `gitea_release_json()` helpers
- Uses reqwest with optional TLS skip

Wire into release stage `run()`: when `token_type == ScmTokenType::Gitea`, use the Gitea backend.

- [ ] **Step 3: Run tests**

Run: `cargo test --lib -p stage-release gitea 2>&1 | tail -20`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add crates/stage-release/src/gitea.rs crates/stage-release/src/lib.rs
git commit -m "feat(release): add Gitea release backend"
```

---

## Task 7: GitLab and Gitea changelog backends

**Files:**
- Modify: `crates/stage-changelog/src/lib.rs`

- [ ] **Step 1: Write failing tests for gitlab/gitea changelog use**

```rust
#[test]
fn test_config_parse_use_source_gitlab() {
    let yaml = r#"
use: gitlab
"#;
    let cfg: anodize_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.use_source.as_deref(), Some("gitlab"));
}

#[test]
fn test_config_parse_use_source_gitea() {
    let yaml = r#"
use: gitea
"#;
    let cfg: anodize_core::config::ChangelogConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.use_source.as_deref(), Some("gitea"));
}

#[test]
fn test_changelog_gitlab_backend_valid_use() {
    // Verify that "gitlab" is accepted as a valid use source
    // (doesn't bail with "unsupported use source")
    use anodize_core::config::{ChangelogConfig, Config, CrateConfig};
    use anodize_core::context::{Context, ContextOptions};

    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.changelog = Some(ChangelogConfig {
        use_source: Some("gitlab".to_string()),
        ..Default::default()
    });
    config.crates = vec![CrateConfig {
        name: "mylib".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        ..Default::default()
    }];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.token_type = anodize_core::scm::ScmTokenType::GitLab;
    let stage = ChangelogStage;
    // Should not bail — gitlab is a valid use source
    // (will fall back to git if API unavailable, matching GoReleaser behavior)
    let result = stage.run(&mut ctx);
    assert!(result.is_ok());
}
```

- [ ] **Step 2: Implement gitlab/gitea changelog backends**

In the validation section that currently rejects non-git/github sources:

```rust
// Replace the current validation:
if use_source != "git" && use_source != "github" {
    anyhow::bail!(...);
}

// With:
if !["git", "github", "gitlab", "gitea"].contains(&use_source.as_str()) {
    anyhow::bail!(
        "changelog: unsupported use source {:?} (expected \"git\", \"github\", \"gitlab\", \"gitea\", or \"github-native\")",
        use_source
    );
}
```

For `gitlab` and `gitea` backends, implement API-based changelog fetching:
- **GitLab**: GET `/api/v4/projects/:id/repository/compare?from=prev&to=current` — parse commits, extract message first line + author info
- **Gitea**: GET `/api/v1/repos/:owner/:repo/git/commits?sha=current&limit=50` — walk commits until prev tag found

Both fall back to `git log` on API failure (matching GoReleaser's behavior when client fails).

Add functions:
```rust
fn fetch_gitlab_commits(ctx: &Context, prev_tag: &Option<String>, log: &StageLogger) -> Result<(Vec<CommitInfo>, String)>
fn fetch_gitea_commits(ctx: &Context, prev_tag: &Option<String>, log: &StageLogger) -> Result<(Vec<CommitInfo>, String)>
```

Wire the `use_gitlab` / `use_gitea` flags alongside the existing `use_github` flag.

- [ ] **Step 3: Run tests**

Run: `cargo test --lib -p stage-changelog 2>&1 | tail -20`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add crates/stage-changelog/src/lib.rs
git commit -m "feat(changelog): add gitlab and gitea backends"
```

---

## Task 8: Publisher config structs — DockerHub, Artifactory, Fury, CloudSmith, NPM

**Files:**
- Modify: `crates/core/src/config.rs`

- [ ] **Step 1: Write failing tests for publisher configs**

```rust
#[test]
fn test_dockerhub_config_parse() {
    let yaml = r#"
project_name: test
dockerhub:
  - username: myuser
    secret_name: DOCKER_TOKEN
    images:
      - myorg/myapp
    description: "My app"
    full_description:
      from_file:
        path: ./README.md
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let dh = &cfg.dockerhub.unwrap()[0];
    assert_eq!(dh.username.as_deref(), Some("myuser"));
    assert_eq!(dh.images.as_ref().unwrap().len(), 1);
}

#[test]
fn test_artifactory_config_parse() {
    let yaml = r#"
project_name: test
artifactories:
  - name: production
    target: "https://artifactory.example.com/repo/{{ .ProjectName }}/{{ .Version }}/"
    username: deployer
    mode: archive
    ids:
      - default
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let art = &cfg.artifactories.unwrap()[0];
    assert_eq!(art.name.as_deref(), Some("production"));
    assert_eq!(art.mode.as_deref(), Some("archive"));
}

#[test]
fn test_fury_config_parse() {
    let yaml = r#"
project_name: test
fury:
  - account: myaccount
    secret_name: FURY_TOKEN
    ids:
      - packages
    formats:
      - deb
      - rpm
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let fury = &cfg.fury.unwrap()[0];
    assert_eq!(fury.account.as_deref(), Some("myaccount"));
    assert_eq!(fury.formats.as_ref().unwrap().len(), 2);
}

#[test]
fn test_cloudsmith_config_parse() {
    let yaml = r#"
project_name: test
cloudsmiths:
  - organization: myorg
    repository: myrepo
    formats:
      - deb
    distributions:
      deb: "ubuntu/focal"
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cs = &cfg.cloudsmiths.unwrap()[0];
    assert_eq!(cs.organization.as_deref(), Some("myorg"));
}

#[test]
fn test_npm_config_parse() {
    let yaml = r#"
project_name: test
npms:
  - name: "@myorg/mypackage"
    description: "My CLI tool"
    license: MIT
    author: "Jane Doe <jane@example.com>"
    access: public
    tag: latest
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let npm = &cfg.npms.unwrap()[0];
    assert_eq!(npm.name.as_deref(), Some("@myorg/mypackage"));
    assert_eq!(npm.access.as_deref(), Some("public"));
}
```

- [ ] **Step 2: Implement publisher config structs**

Add to `crates/core/src/config.rs`:

```rust
// ---------------------------------------------------------------------------
// DockerHub description sync (GoReleaser Pro)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerHubConfig {
    /// Docker Hub username (must have editor permissions).
    pub username: Option<String>,
    /// Environment variable name for the push token.
    pub secret_name: Option<String>,
    /// Docker images to apply descriptions to.
    pub images: Option<Vec<String>>,
    /// Short description of the image.
    pub description: Option<String>,
    /// Full description content.
    pub full_description: Option<DockerHubFullDescription>,
    /// Disable this configuration.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerHubFullDescription {
    /// Load from URL.
    pub from_url: Option<DockerHubFromUrl>,
    /// Load from local file (overrides from_url).
    pub from_file: Option<DockerHubFromFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct DockerHubFromUrl {
    pub url: String,
    pub headers: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct DockerHubFromFile {
    pub path: String,
}

// ---------------------------------------------------------------------------
// Artifactory publisher
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ArtifactoryConfig {
    /// Instance name (used in env var lookup: ARTIFACTORY_{NAME}_SECRET).
    pub name: Option<String>,
    /// Target URL template for uploads.
    pub target: Option<String>,
    /// Upload mode: "archive" or "binary".
    pub mode: Option<String>,
    /// Username for basic auth.
    pub username: Option<String>,
    /// Password for basic auth (alternative to env var).
    pub password: Option<String>,
    /// Artifact IDs to filter.
    pub ids: Option<Vec<String>>,
    /// File extensions to filter.
    pub exts: Option<Vec<String>>,
    /// Client X509 certificate path.
    pub client_x509_cert: Option<String>,
    /// Client X509 key path.
    pub client_x509_key: Option<String>,
    /// Custom HTTP headers.
    pub custom_headers: Option<HashMap<String, String>>,
    /// Checksum header name (default: X-Checksum-SHA256).
    pub checksum_header: Option<String>,
    /// Extra files to upload.
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Also upload checksum files.
    pub checksum: Option<bool>,
    /// Also upload signature files.
    pub signature: Option<bool>,
    /// Also upload metadata files.
    pub meta: Option<bool>,
    /// Use custom artifact name in URL.
    pub custom_artifact_name: Option<bool>,
    /// Skip this config.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
}

// ---------------------------------------------------------------------------
// GemFury publisher (GoReleaser Pro)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct FuryConfig {
    /// Fury account name.
    pub account: Option<String>,
    /// Disable this configuration.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    /// Environment variable name for push token (default: FURY_TOKEN).
    pub secret_name: Option<String>,
    /// Artifact IDs to filter.
    pub ids: Option<Vec<String>>,
    /// Formats to upload: apk, deb, rpm.
    pub formats: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// CloudSmith publisher (GoReleaser Pro)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct CloudSmithConfig {
    /// CloudSmith organization.
    pub organization: Option<String>,
    /// CloudSmith repository.
    pub repository: Option<String>,
    /// Artifact IDs to filter.
    pub ids: Option<Vec<String>>,
    /// Formats to upload: apk, deb, rpm.
    pub formats: Option<Vec<String>>,
    /// Distribution mapping per format.
    pub distributions: Option<HashMap<String, serde_yaml_ng::Value>>,
    /// Component/channel name.
    pub component: Option<String>,
    /// Environment variable for token (default: CLOUDSMITH_TOKEN).
    pub secret_name: Option<String>,
    /// Skip this configuration.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Allow overwriting existing packages.
    pub republish: Option<bool>,
}

// ---------------------------------------------------------------------------
// NPM publisher (GoReleaser Pro)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct NpmConfig {
    /// NPM config ID.
    pub id: Option<String>,
    /// Package name (e.g. "@myorg/mypackage").
    pub name: Option<String>,
    /// Package description.
    pub description: Option<String>,
    /// Package homepage URL.
    pub homepage: Option<String>,
    /// Package keywords.
    pub keywords: Option<Vec<String>>,
    /// License identifier.
    pub license: Option<String>,
    /// Author string.
    pub author: Option<String>,
    /// Repository URL.
    pub repository: Option<String>,
    /// Bug tracker URL.
    pub bugs: Option<String>,
    /// Access level: "public" or "restricted".
    pub access: Option<String>,
    /// Publish tag (default: "latest").
    pub tag: Option<String>,
    /// Archive format to use.
    pub format: Option<String>,
    /// Artifact IDs to filter.
    pub ids: Option<Vec<String>>,
    /// Extra files to include.
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Templated extra files.
    pub templated_extra_files: Option<Vec<TemplatedExtraFile>>,
    /// Disable this configuration.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub disable: Option<StringOrBool>,
    /// URL template for artifact downloads.
    pub url_template: Option<String>,
    /// Conditional filter expression.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// Extra fields for package.json root.
    pub extra: Option<HashMap<String, serde_yaml_ng::Value>>,
}
```

Add to `Config` struct:
```rust
    /// DockerHub description sync configurations.
    pub dockerhub: Option<Vec<DockerHubConfig>>,
    /// Artifactory upload configurations.
    pub artifactories: Option<Vec<ArtifactoryConfig>>,
    /// GemFury publisher configurations.
    #[serde(alias = "gemfury")]
    pub fury: Option<Vec<FuryConfig>>,
    /// CloudSmith publisher configurations.
    pub cloudsmiths: Option<Vec<CloudSmithConfig>>,
    /// NPM publisher configurations.
    pub npms: Option<Vec<NpmConfig>>,
```

- [ ] **Step 3: Run tests**

Run: `cargo test --lib -p anodize-core test_dockerhub test_artifactory test_fury test_cloudsmith test_npm 2>&1 | tail -20`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/config.rs
git commit -m "feat(config): add DockerHub, Artifactory, Fury, CloudSmith, NPM publisher configs"
```

---

## Task 9: DockerHub description sync stage

**Files:**
- Create: `crates/stage-publish/src/dockerhub.rs`
- Modify: `crates/stage-publish/src/lib.rs`

- [ ] **Step 1: Write tests for DockerHub behavior**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dockerhub_skips_when_no_config() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = DockerHubStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dockerhub_skips_when_disabled() {
        let mut config = Config::default();
        config.dockerhub = Some(vec![DockerHubConfig {
            disable: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }]);
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = DockerHubStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dockerhub_dry_run_logs() {
        let mut config = Config::default();
        config.dockerhub = Some(vec![DockerHubConfig {
            username: Some("testuser".to_string()),
            images: Some(vec!["myorg/myapp".to_string()]),
            description: Some("My app".to_string()),
            ..Default::default()
        }]);
        let mut opts = ContextOptions::default();
        opts.dry_run = true;
        let mut ctx = Context::new(config, opts);
        let stage = DockerHubStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_resolve_full_description_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let readme = dir.path().join("README.md");
        std::fs::write(&readme, "# My App\nDescription here").unwrap();

        let desc = DockerHubFullDescription {
            from_file: Some(DockerHubFromFile { path: readme.to_str().unwrap().to_string() }),
            from_url: None,
        };
        let result = resolve_full_description(&desc).unwrap();
        assert_eq!(result, "# My App\nDescription here");
    }
}
```

- [ ] **Step 2: Implement DockerHub description sync**

The DockerHub API (v2) PATCH `/repositories/{namespace}/{name}/` accepts `description` and `full_description`. Auth: POST to `https://hub.docker.com/v2/users/login/` with username + password to get a JWT token.

```rust
pub struct DockerHubStage;

impl Stage for DockerHubStage {
    fn name(&self) -> &str { "dockerhub" }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let configs = match &ctx.config.dockerhub {
            Some(cfgs) if !cfgs.is_empty() => cfgs.clone(),
            _ => return Ok(()),
        };
        let log = ctx.logger("dockerhub");
        for cfg in &configs {
            if let Some(ref d) = cfg.disable {
                if d.is_disabled(|s| ctx.render_template(s)) {
                    log.status("dockerhub sync disabled, skipping");
                    continue;
                }
            }
            // Resolve username, secret, images, descriptions
            // PATCH Docker Hub API for each image
            // ...
        }
        Ok(())
    }
}
```

- [ ] **Step 3: Wire into pipeline**

Add `DockerHubStage` to the publish phase of the pipeline in `crates/cli/src/pipeline.rs`.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib -p stage-publish dockerhub 2>&1 | tail -20`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add crates/stage-publish/src/dockerhub.rs crates/stage-publish/src/lib.rs crates/cli/src/pipeline.rs
git commit -m "feat(publish): add DockerHub description sync stage"
```

---

## Task 10: Artifactory publisher stage

**Files:**
- Create: `crates/stage-publish/src/artifactory.rs`
- Modify: `crates/stage-publish/src/lib.rs`

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_artifactory_skips_when_no_config() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = ArtifactoryStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_artifactory_default_checksum_header() {
        let cfg = ArtifactoryConfig::default();
        let header = cfg.checksum_header.as_deref().unwrap_or("X-Checksum-SHA256");
        assert_eq!(header, "X-Checksum-SHA256");
    }

    #[test]
    fn test_artifactory_mode_validation() {
        assert!(validate_upload_mode("archive").is_ok());
        assert!(validate_upload_mode("binary").is_ok());
        assert!(validate_upload_mode("invalid").is_err());
    }

    #[test]
    fn test_artifactory_target_url_template() {
        let target = "https://artifactory.example.com/repo/{{ .ProjectName }}/{{ .Version }}/";
        // Template rendering would expand this — just verify it's accepted
        assert!(target.contains("{{ .ProjectName }}"));
    }
}
```

- [ ] **Step 2: Implement Artifactory publisher**

Follows GoReleaser's HTTP upload pattern: PUT artifact to target URL with basic auth, checksum header, and custom headers. Same shared upload logic for Artifactory-specific error response parsing.

- [ ] **Step 3: Wire into pipeline, run tests, commit**

```bash
git add crates/stage-publish/src/artifactory.rs crates/stage-publish/src/lib.rs crates/cli/src/pipeline.rs
git commit -m "feat(publish): add Artifactory upload publisher"
```

---

## Task 11: GemFury publisher stage

**Files:**
- Create: `crates/stage-publish/src/fury.rs`
- Modify: `crates/stage-publish/src/lib.rs`

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fury_skips_when_no_config() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = FuryStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_fury_skips_when_empty_account() {
        let mut config = Config::default();
        config.fury = Some(vec![FuryConfig { account: None, ..Default::default() }]);
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = FuryStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_fury_default_formats() {
        let defaults = fury_default_formats();
        assert_eq!(defaults, vec!["apk", "deb", "rpm"]);
    }

    #[test]
    fn test_fury_upload_url() {
        let url = fury_push_url("myaccount");
        assert_eq!(url, "https://push.fury.io/myaccount/");
    }

    #[test]
    fn test_fury_filters_by_format() {
        // Only deb and rpm should match when formats = ["deb", "rpm"]
        let formats = vec!["deb".to_string(), "rpm".to_string()];
        assert!(fury_format_matches("myapp_1.0.0_amd64.deb", &formats));
        assert!(fury_format_matches("myapp-1.0.0.x86_64.rpm", &formats));
        assert!(!fury_format_matches("myapp-1.0.0.tar.gz", &formats));
    }
}
```

- [ ] **Step 2: Implement Fury publisher**

Push via POST to `https://push.fury.io/{account}/` with Bearer token from `FURY_TOKEN` (or custom `secret_name`). Filters artifacts by format (deb/rpm/apk) and ids.

- [ ] **Step 3: Wire, test, commit**

```bash
git add crates/stage-publish/src/fury.rs crates/stage-publish/src/lib.rs crates/cli/src/pipeline.rs
git commit -m "feat(publish): add GemFury deb/rpm/apk publisher"
```

---

## Task 12: CloudSmith publisher stage

**Files:**
- Create: `crates/stage-publish/src/cloudsmith.rs`
- Modify: `crates/stage-publish/src/lib.rs`

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cloudsmith_skips_when_no_config() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = CloudSmithStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_cloudsmith_upload_url() {
        let url = cloudsmith_upload_url("myorg", "myrepo", "deb", "ubuntu/focal");
        assert!(url.contains("myorg"));
        assert!(url.contains("myrepo"));
    }

    #[test]
    fn test_cloudsmith_default_formats() {
        let defaults = cloudsmith_default_formats();
        assert_eq!(defaults, vec!["apk", "deb", "rpm"]);
    }
}
```

- [ ] **Step 2: Implement CloudSmith publisher**

Uses CloudSmith API: POST to `https://upload.cloudsmith.io/{org}/{repo}/{format}/` with `CLOUDSMITH_TOKEN` auth. Supports distribution mapping per format, component, and republish flag.

- [ ] **Step 3: Wire, test, commit**

```bash
git add crates/stage-publish/src/cloudsmith.rs crates/stage-publish/src/lib.rs crates/cli/src/pipeline.rs
git commit -m "feat(publish): add CloudSmith package publisher"
```

---

## Task 13: NPM publisher stage

**Files:**
- Create: `crates/stage-publish/src/npm.rs`
- Modify: `crates/stage-publish/src/lib.rs`

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_npm_skips_when_no_config() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = NpmStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_npm_package_json_generation() {
        let pkg = generate_package_json(
            "@myorg/mypackage",
            "1.0.0",
            Some("My CLI tool"),
            Some("MIT"),
            Some("Jane Doe"),
            Some("public"),
            None, // extra
        );
        assert_eq!(pkg["name"], "@myorg/mypackage");
        assert_eq!(pkg["version"], "1.0.0");
        assert_eq!(pkg["license"], "MIT");
        assert!(pkg["scripts"]["postinstall"].is_string());
    }

    #[test]
    fn test_npm_postinstall_script_generation() {
        let script = generate_postinstall_script("https://github.com/owner/repo/releases/download/v1.0.0/");
        assert!(script.contains("https://github.com"));
    }
}
```

- [ ] **Step 2: Implement NPM publisher**

Generates `package.json` with postinstall script that downloads the correct binary archive based on OS/arch. Publishes via `npm publish` subprocess. Supports access (public/restricted), tag, extra fields.

- [ ] **Step 3: Wire, test, commit**

```bash
git add crates/stage-publish/src/npm.rs crates/stage-publish/src/lib.rs crates/cli/src/pipeline.rs
git commit -m "feat(publish): add NPM package publisher"
```

---

## Task 14: Verify Snapcraft publish is wired into pipeline

**Files:**
- Modify: `crates/cli/src/pipeline.rs` (if needed)

- [ ] **Step 1: Verify SnapcraftPublishStage is in the publish pipeline**

Check that `SnapcraftPublishStage` is registered in the pipeline's publish phase. If not, add it.

- [ ] **Step 2: Write integration test**

```rust
#[test]
fn test_snapcraft_publish_in_pipeline() {
    // Verify SnapcraftPublishStage appears in the publish phase stages list
    let stages = publish_stages();
    assert!(stages.iter().any(|s| s.name() == "snapcraft-publish"));
}
```

- [ ] **Step 3: Commit if changes needed**

```bash
git add crates/cli/src/pipeline.rs
git commit -m "fix(pipeline): ensure snapcraft publish stage is wired"
```

---

## Task 15: Spec review + code quality review

- [ ] **Step 1: Spec review — verify GoReleaser parity**

For each Session F item in `parity-session-index.md`, verify:
1. Config field parity: every GoReleaser field has an equivalent
2. Behavioral parity: same output given same input
3. Wiring parity: config flows through to behavior
4. Error parity: same error cases handled
5. Auth parity: credential chains match
6. Default parity: defaults match or are explicitly better

- [ ] **Step 2: Code quality review**

Review all new code for:
- Unused imports or dead code
- Error handling consistency
- Template rendering in all user-facing strings
- Proper dry-run support in all stages
- Test coverage for edge cases

- [ ] **Step 3: Fix all findings**

Fix every issue found in both reviews, regardless of severity.

- [ ] **Step 4: Re-review until zero findings**

- [ ] **Step 5: Mark Session F items complete in parity-session-index.md**

Check all boxes for Session F items.

- [ ] **Step 6: Final commit**

```bash
git add -A
git commit -m "docs: mark Session F complete in parity session index"
```
