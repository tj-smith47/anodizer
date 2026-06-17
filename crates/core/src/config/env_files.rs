use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

// ---------------------------------------------------------------------------
// EnvFilesConfig — accepts list of .env paths OR structured token file paths
// ---------------------------------------------------------------------------

/// Environment file configuration.
///
/// Accepts two forms:
/// - **List form** (anodizer extension): array of `.env` file paths loaded as KEY=VALUE.
///   ```yaml
///   env_files:
///     - .env
///     - .release.env
///   ```
/// - **Struct form**: paths to files containing provider tokens.
///   ```yaml
///   env_files:
///     github_token: ~/.config/goreleaser/github_token
///     gitlab_token: ~/.config/goreleaser/gitlab_token
///     gitea_token: ~/.config/goreleaser/gitea_token
///   ```
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum EnvFilesConfig {
    /// List of `.env` file paths to load (KEY=VALUE format).
    List(Vec<String>),
    /// Structured token file paths.
    TokenFiles(EnvFilesTokenConfig),
}

impl<'de> Deserialize<'de> for EnvFilesConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_yaml_ng::Value::deserialize(deserializer)?;
        match &value {
            serde_yaml_ng::Value::Sequence(_) => {
                let list: Vec<String> =
                    serde_yaml_ng::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(EnvFilesConfig::List(list))
            }
            serde_yaml_ng::Value::Mapping(_) => {
                let tokens: EnvFilesTokenConfig =
                    serde_yaml_ng::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(EnvFilesConfig::TokenFiles(tokens))
            }
            _ => Err(serde::de::Error::custom(
                "env_files must be an array of file paths or a mapping with token file paths",
            )),
        }
    }
}

impl EnvFilesConfig {
    /// Returns the list of .env file paths if this is the List variant.
    pub fn as_list(&self) -> Option<&[String]> {
        match self {
            EnvFilesConfig::List(files) => Some(files),
            EnvFilesConfig::TokenFiles(_) => None,
        }
    }

    /// Returns the token files config if this is the TokenFiles variant.
    pub fn as_token_files(&self) -> Option<&EnvFilesTokenConfig> {
        match self {
            EnvFilesConfig::List(_) => None,
            EnvFilesConfig::TokenFiles(tokens) => Some(tokens),
        }
    }
}

/// Structured token file paths for provider authentication.
///
/// Each field points to a file containing a single-line token. When present,
/// the file is read and the corresponding environment variable is set
/// (e.g., `github_token` file -> `GITHUB_TOKEN` env var).
///
/// Token-file path overrides.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EnvFilesTokenConfig {
    /// Path to file containing the GitHub token. Default: `~/.config/goreleaser/github_token`.
    pub github_token: Option<String>,
    /// Path to file containing the GitLab token. Default: `~/.config/goreleaser/gitlab_token`.
    pub gitlab_token: Option<String>,
    /// Path to file containing the Gitea token. Default: `~/.config/goreleaser/gitea_token`.
    pub gitea_token: Option<String>,
}

/// Read a single token from a file, returning the first line trimmed.
///
/// Returns `Ok(None)` if the file does not exist.
/// Returns `Err` if the file exists but cannot be read.
pub fn read_token_file(path: &str) -> Result<Option<String>, String> {
    read_token_file_with_env(path, &crate::ProcessEnvSource)
}

/// [`EnvSource`](crate::EnvSource)-injecting form of [`read_token_file`].
///
/// Tilde expansion resolves the home directory from `env` instead of the
/// process environment, so tests can point `~` at a fixture directory
/// without mutating global `HOME`.
pub fn read_token_file_with_env<E: crate::EnvSource + ?Sized>(
    path: &str,
    env: &E,
) -> Result<Option<String>, String> {
    let expanded = crate::path_util::expand_tilde_with_env(path, env);

    match std::fs::read_to_string(expanded.as_ref()) {
        Ok(content) => {
            let token = content.lines().next().unwrap_or("").trim().to_string();
            if token.is_empty() {
                Ok(None)
            } else {
                Ok(Some(token))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("failed to read token file '{}': {}", path, e)),
    }
}

/// Load tokens from structured `env_files` config.
///
/// For each configured token file path, reads the file and returns the
/// corresponding environment variable name and token value.
/// Falls back to the default token-file paths (`~/.config/goreleaser/...`) when
/// a field is not specified.
///
/// Only returns entries where the corresponding process env var is NOT already
/// set (env var takes precedence).
pub fn load_token_files(
    config: &EnvFilesTokenConfig,
    log: &crate::log::StageLogger,
) -> Result<std::collections::HashMap<String, String>, String> {
    load_token_files_with_env(config, log, &crate::ProcessEnvSource)
}

/// [`EnvSource`](crate::EnvSource)-injecting form of [`load_token_files`].
///
/// The "process env var takes precedence over the token file" check and the
/// `~`-expansion of candidate paths both read through `env`, so tests can
/// drive token precedence and home-relative paths deterministically without
/// mutating the process environment.
pub fn load_token_files_with_env<E: crate::EnvSource + ?Sized>(
    config: &EnvFilesTokenConfig,
    log: &crate::log::StageLogger,
    env: &E,
) -> Result<std::collections::HashMap<String, String>, String> {
    let mut vars = std::collections::HashMap::new();

    // Per-token candidate paths. The user's explicit `github_token` / etc.
    // config value wins if present; otherwise we try anodizer-native first,
    // then the conventional path for users migrating in.
    let github_candidates: Vec<&str> = match config.github_token.as_deref() {
        Some(p) => vec![p],
        None => vec![
            "~/.config/anodizer/github_token",
            "~/.config/goreleaser/github_token",
        ],
    };
    let gitlab_candidates: Vec<&str> = match config.gitlab_token.as_deref() {
        Some(p) => vec![p],
        None => vec![
            "~/.config/anodizer/gitlab_token",
            "~/.config/goreleaser/gitlab_token",
        ],
    };
    let gitea_candidates: Vec<&str> = match config.gitea_token.as_deref() {
        Some(p) => vec![p],
        None => vec![
            "~/.config/anodizer/gitea_token",
            "~/.config/goreleaser/gitea_token",
        ],
    };
    let mappings: [(&str, &[&str]); 3] = [
        ("GITHUB_TOKEN", &github_candidates),
        ("GITLAB_TOKEN", &gitlab_candidates),
        ("GITEA_TOKEN", &gitea_candidates),
    ];

    for (env_name, candidates) in &mappings {
        // Skip if the env var is already set in the process environment
        if env.var(env_name).filter(|v| !v.is_empty()).is_some() {
            log.verbose(&format!("using {} from process environment", env_name));
            continue;
        }
        for file_path in candidates.iter() {
            match read_token_file_with_env(file_path, env) {
                Ok(Some(token)) => {
                    log.verbose(&format!("loaded {} from {}", env_name, file_path));
                    vars.insert(env_name.to_string(), token);
                    break;
                }
                Ok(None) => {
                    // File doesn't exist or is empty — try next candidate
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
    }

    Ok(vars)
}

/// Load environment variables from .env-style files.
/// Each file is read as KEY=VALUE lines. Lines starting with # and empty lines are skipped.
/// Returns a HashMap of parsed key-value pairs. Does NOT mutate the process
/// environment — callers should inject these into the template context via
/// `set_env()` and pass them to subprocesses via `Command::envs()`.
pub fn load_env_files(
    files: &[String],
    log: &crate::log::StageLogger,
    strict: bool,
) -> Result<std::collections::HashMap<String, String>, String> {
    let mut vars = std::collections::HashMap::new();
    for file_path in files {
        let content = match std::fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if strict {
                    return Err(format!("env file '{}' not found (strict mode)", file_path));
                }
                log.warn(&format!("skipped env file '{}' — not found", file_path));
                continue;
            }
            Err(e) => {
                return Err(format!("failed to read env file '{}': {}", file_path, e));
            }
        };
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            // Strip `export ` prefix (common in .env files)
            let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
            if let Some((key, value)) = trimmed.split_once('=') {
                let key = key.trim();
                if key.is_empty() {
                    log.warn(&format!(
                        "skipped line — empty key in '{}': {}",
                        file_path,
                        line.trim()
                    ));
                    continue;
                }
                let value = value.trim();
                // Strip surrounding quotes from value if present
                let value = if value.len() >= 2
                    && ((value.starts_with('"') && value.ends_with('"'))
                        || (value.starts_with('\'') && value.ends_with('\'')))
                {
                    &value[1..value.len() - 1]
                } else {
                    value
                };
                vars.insert(key.to_string(), value.to_string());
            } else {
                log.warn(&format!(
                    "skipped line — no '=' in '{}': {}",
                    file_path, trimmed
                ));
            }
        }
    }
    Ok(vars)
}

// ---------------------------------------------------------------------------
// env helpers — Vec<String> of "KEY=VAL" entries
// ---------------------------------------------------------------------------
//
// Lifted to `crate::env` so they are reachable as
// `anodizer_core::env::*` directly. The re-exports below preserve the
// historical `anodizer_core::config::*` import paths used by stages and
// publishers.

pub use crate::env::{parse_env_entries, render_env_entries, split_env_entry};
