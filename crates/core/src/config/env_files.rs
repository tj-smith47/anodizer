use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

// EnvFilesConfig — accepts list of .env paths OR structured token file paths

/// Environment file configuration.
///
/// Accepts two forms:
/// - **List form** (anodizer extension): array of `.env` file paths loaded as KEY=VALUE.
///   The token files are not read in this form. Once the run has chosen its
///   forge, a `GITHUB_TOKEN` (or `GH_TOKEN`), `GITLAB_TOKEN` or
///   `GITEA_TOKEN` line for that forge in one of the files is the run token
///   when the forge's variables are unset or blank. The line never chooses
///   the forge, and a line for another forge is only template env.
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
    /// Path to file containing the GitHub token. Default: `~/.config/anodizer/github_token`,
    /// then `~/.config/goreleaser/github_token`.
    pub github_token: Option<String>,
    /// Path to file containing the GitLab token. Default: `~/.config/anodizer/gitlab_token`,
    /// then `~/.config/goreleaser/gitlab_token`.
    pub gitlab_token: Option<String>,
    /// Path to file containing the Gitea token. Default: `~/.config/anodizer/gitea_token`,
    /// then `~/.config/goreleaser/gitea_token`.
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
/// Tilde expansion resolves the home directory from `env`, which need not be the
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
/// Falls back to the default token-file paths (`~/.config/anodizer/...`, then
/// `~/.config/goreleaser/...`) when a field is not specified.
///
/// A token file is the last source of its token: the GitHub file is read only
/// when `ANODIZER_GITHUB_TOKEN`, `GITHUB_TOKEN` and `GH_TOKEN` are all unset or
/// blank, and the GitLab and Gitea files only when `GITLAB_TOKEN` or
/// `GITEA_TOKEN` is unset or blank.
pub fn load_token_files(
    config: &EnvFilesTokenConfig,
    log: &crate::log::StageLogger,
) -> Result<std::collections::HashMap<String, String>, String> {
    load_token_files_with_env(config, log, &crate::ProcessEnvSource)
}

/// The token file read for the GitHub token when `env_files.github_token`
/// is unset.
pub const DEFAULT_GITHUB_TOKEN_FILE: &str = "~/.config/anodizer/github_token";
/// The token file read for the GitLab token when `env_files.gitlab_token`
/// is unset.
pub const DEFAULT_GITLAB_TOKEN_FILE: &str = "~/.config/anodizer/gitlab_token";
/// The token file read for the Gitea token when `env_files.gitea_token` is
/// unset.
pub const DEFAULT_GITEA_TOKEN_FILE: &str = "~/.config/anodizer/gitea_token";

/// The variables a forge's run token is read from, in the order they are
/// read: `ANODIZER_GITHUB_TOKEN`, `GITHUB_TOKEN`, `GH_TOKEN` for GitHub,
/// `GITLAB_TOKEN` for GitLab and `GITEA_TOKEN` for Gitea.
pub fn forge_token_vars(forge: crate::scm::ScmTokenType) -> &'static [&'static str] {
    match forge {
        crate::scm::ScmTokenType::GitHub => crate::git::GITHUB_TOKEN_ENV_LADDER,
        crate::scm::ScmTokenType::GitLab => &["GITLAB_TOKEN"],
        crate::scm::ScmTokenType::Gitea => &["GITEA_TOKEN"],
    }
}

/// The variable a forge's token is held under once it is read from a file:
/// `GITHUB_TOKEN`, `GITLAB_TOKEN` or `GITEA_TOKEN`.
pub fn forge_file_var(forge: crate::scm::ScmTokenType) -> &'static str {
    match forge {
        crate::scm::ScmTokenType::GitHub => "GITHUB_TOKEN",
        crate::scm::ScmTokenType::GitLab => "GITLAB_TOKEN",
        crate::scm::ScmTokenType::Gitea => "GITEA_TOKEN",
    }
}

/// The file source of a forge's token as the config reads it, worded as the
/// last clause of a missing-token hint, or `None` when the config reads no
/// file for it (`env_files: []`).
///
/// - With `env_files` unset, or in its mapping form, the clause names the one
///   path a user writes: the configured `env_files.<forge>_token`, else the
///   default (`write the token to ~/.config/anodizer/github_token`).
/// - In the list form no token file is read, so the clause names the env
///   files (`put a GITHUB_TOKEN line in one of the env files: .env,
///   .release.env`).
pub fn token_file_clause(
    env_files: Option<&EnvFilesConfig>,
    forge: crate::scm::ScmTokenType,
) -> Option<String> {
    use crate::scm::ScmTokenType;
    match env_files {
        Some(EnvFilesConfig::List(files)) if files.is_empty() => None,
        Some(EnvFilesConfig::List(files)) => Some(format!(
            "put a {} line in one of the env files: {}",
            forge_file_var(forge),
            files.join(", ")
        )),
        Some(EnvFilesConfig::TokenFiles(_)) | None => {
            let paths = token_file_config(env_files).unwrap_or_default();
            let path = match forge {
                ScmTokenType::GitHub => paths.github_token.as_deref(),
                ScmTokenType::GitLab => paths.gitlab_token.as_deref(),
                ScmTokenType::Gitea => paths.gitea_token.as_deref(),
            }
            .unwrap_or(match forge {
                ScmTokenType::GitHub => DEFAULT_GITHUB_TOKEN_FILE,
                ScmTokenType::GitLab => DEFAULT_GITLAB_TOKEN_FILE,
                ScmTokenType::Gitea => DEFAULT_GITEA_TOKEN_FILE,
            });
            Some(format!("write the token to {path}"))
        }
    }
}

/// The remediation hint for a missing forge token, naming every source the
/// run reads for that forge under this config, in the order it reads them:
/// `set GITLAB_TOKEN, pass --token, or write the token to
/// ~/.config/anodizer/gitlab_token`. The file clause is
/// [`token_file_clause`].
pub fn missing_token_hint(
    env_files: Option<&EnvFilesConfig>,
    forge: crate::scm::ScmTokenType,
) -> String {
    let vars = forge_token_vars(forge).join(" or ");
    match token_file_clause(env_files, forge) {
        Some(file) => format!("set {vars}, pass --token, or {file}"),
        None => format!("set {vars} or pass --token"),
    }
}

impl super::Config {
    /// [`missing_token_hint`] for this config's `env_files`.
    pub fn missing_token_hint(&self, forge: crate::scm::ScmTokenType) -> String {
        missing_token_hint(self.env_files.as_ref(), forge)
    }
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
    // config value wins if present; otherwise the anodizer-native path comes first,
    // then the conventional path for users migrating in.
    let github_candidates: Vec<&str> = match config.github_token.as_deref() {
        Some(p) => vec![p],
        None => vec![
            DEFAULT_GITHUB_TOKEN_FILE,
            "~/.config/goreleaser/github_token",
        ],
    };
    let gitlab_candidates: Vec<&str> = match config.gitlab_token.as_deref() {
        Some(p) => vec![p],
        None => vec![
            DEFAULT_GITLAB_TOKEN_FILE,
            "~/.config/goreleaser/gitlab_token",
        ],
    };
    let gitea_candidates: Vec<&str> = match config.gitea_token.as_deref() {
        Some(p) => vec![p],
        None => vec![DEFAULT_GITEA_TOKEN_FILE, "~/.config/goreleaser/gitea_token"],
    };
    let mappings: [(&str, &[&str], &[&str]); 3] = [
        (
            "GITHUB_TOKEN",
            &github_candidates,
            crate::git::GITHUB_TOKEN_ENV_LADDER,
        ),
        ("GITLAB_TOKEN", &gitlab_candidates, &["GITLAB_TOKEN"]),
        ("GITEA_TOKEN", &gitea_candidates, &["GITEA_TOKEN"]),
    ];

    for (env_name, candidates, readers) in &mappings {
        if let Some(set) = readers.iter().find(|name| {
            env.var(name)
                .as_deref()
                .and_then(crate::git::non_blank_token)
                .is_some()
        }) {
            log.verbose(&format!("using {set} from process environment"));
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

/// The token-file paths a config reads: the `env_files` mapping, or the
/// default paths when `env_files` is not set. The list form of `env_files`
/// names `.env` files and reads no token file.
pub fn token_file_config(env_files: Option<&EnvFilesConfig>) -> Option<EnvFilesTokenConfig> {
    match env_files {
        None => Some(EnvFilesTokenConfig::default()),
        Some(EnvFilesConfig::TokenFiles(paths)) => Some(paths.clone()),
        Some(EnvFilesConfig::List(_)) => None,
    }
}

/// The tokens the config's token files hold, keyed by the variable each one
/// stands for (`GITHUB_TOKEN`, `GITLAB_TOKEN`, `GITEA_TOKEN`), read without
/// logging. Only the tokens [`load_token_files_with_env`] would load are
/// returned, so a file never outranks a variable. The list form of
/// `env_files` reads no token file and returns no token. A file that exists
/// but cannot be read is an error.
///
/// A token held here picks the forge the run releases to, the same as the
/// variable it stands for.
pub fn token_file_tokens<E: crate::EnvSource + ?Sized>(
    env_files: Option<&EnvFilesConfig>,
    env: &E,
) -> Result<std::collections::HashMap<String, String>, String> {
    let Some(paths) = token_file_config(env_files) else {
        return Ok(std::collections::HashMap::new());
    };
    let quiet = crate::log::StageLogger::new("env", crate::log::Verbosity::Quiet);
    load_token_files_with_env(&paths, &quiet, env)
}

/// The token a listed env file holds for `forge`, the last source of that
/// forge's run token: for GitHub the first of `ANODIZER_GITHUB_TOKEN`,
/// `GITHUB_TOKEN` and `GH_TOKEN` the files set to a value that is not blank,
/// for GitLab `GITLAB_TOKEN`, for Gitea `GITEA_TOKEN`. The value is trimmed.
///
/// Only the list form of `env_files` has env files; any other form returns
/// `None`. The caller reads this after the forge is chosen and after its
/// variables and token file give no token, so an env-file line never picks
/// the forge and never outranks a variable. A missing env file is skipped
/// here; the run reports it when it loads the files.
pub fn env_file_token(
    env_files: Option<&EnvFilesConfig>,
    forge: crate::scm::ScmTokenType,
) -> Result<Option<String>, String> {
    let Some(EnvFilesConfig::List(files)) = env_files else {
        return Ok(None);
    };
    let quiet = crate::log::StageLogger::new("env", crate::log::Verbosity::Quiet);
    let lines = load_env_files(files, &quiet, false)?;
    Ok(forge_token_vars(forge)
        .iter()
        .find_map(|name| crate::git::non_blank_token(lines.get(*name)?)))
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

// env helpers — Vec<String> of "KEY=VAL" entries
//
// Lifted to `crate::env` so they are reachable as
// `anodizer_core::env::*` directly. The re-exports below preserve the
// historical `anodizer_core::config::*` import paths used by stages and
// publishers.

pub use crate::env::{
    parse_env_entries, render_env_entries, render_env_entries_that_render, split_env_entry,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn test_log() -> crate::log::StageLogger {
        crate::log::StageLogger::new("test", crate::log::Verbosity::Normal)
    }

    // EnvFilesConfig deserialize dispatch

    #[test]
    fn env_files_scalar_input_is_rejected() {
        // Neither a sequence nor a mapping — the hand-written deserializer must
        // error rather than coerce a bare string.
        let r: Result<EnvFilesConfig, _> = serde_yaml_ng::from_str("just-a-string");
        assert!(r.is_err(), "a scalar env_files value must be rejected");
    }

    #[test]
    fn as_list_and_as_token_files_are_mutually_exclusive() {
        let list = EnvFilesConfig::List(vec![".env".into()]);
        assert_eq!(list.as_list(), Some([".env".to_string()].as_slice()));
        assert!(list.as_token_files().is_none());

        let tokens = EnvFilesConfig::TokenFiles(EnvFilesTokenConfig::default());
        assert!(tokens.as_list().is_none());
        assert!(tokens.as_token_files().is_some());
    }

    // load_token_files candidate precedence

    #[test]
    fn unset_field_prefers_anodizer_path_over_goreleaser() {
        // With no explicit github_token, the anodizer-native path is tried
        // first; when it exists it wins even though the goreleaser path also
        // exists.
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path();
        let anod = home.join(".config/anodizer/github_token");
        let gore = home.join(".config/goreleaser/github_token");
        std::fs::create_dir_all(anod.parent().unwrap()).unwrap();
        std::fs::create_dir_all(gore.parent().unwrap()).unwrap();
        std::fs::write(&anod, "anodizer_tok\n").unwrap();
        std::fs::write(&gore, "goreleaser_tok\n").unwrap();

        let env = crate::MapEnvSource::new().with("HOME", home.to_string_lossy().to_string());
        let vars =
            load_token_files_with_env(&EnvFilesTokenConfig::default(), &test_log(), &env).unwrap();
        assert_eq!(vars.get("GITHUB_TOKEN").unwrap(), "anodizer_tok");
    }

    #[test]
    fn the_github_token_file_is_read_only_when_every_github_variable_is_unset_or_blank() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path();
        let file = home.join(".config/anodizer/github_token");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "file_tok\n").unwrap();
        let home = home.to_string_lossy().to_string();

        for var in crate::git::GITHUB_TOKEN_ENV_LADDER {
            let env = crate::MapEnvSource::new()
                .with("HOME", home.clone())
                .with(*var, "env_tok");
            let tokens = token_file_tokens(None, &env).unwrap();
            assert_eq!(
                tokens.get("GITHUB_TOKEN"),
                None,
                "{var} set: the file is not read"
            );
        }
        let blank = crate::MapEnvSource::new()
            .with("HOME", home.clone())
            .with("GH_TOKEN", "  ")
            .with("GITHUB_TOKEN", "");
        assert_eq!(
            token_file_tokens(None, &blank)
                .unwrap()
                .get("GITHUB_TOKEN")
                .map(String::as_str),
            Some("file_tok"),
        );
        let listed = EnvFilesConfig::List(vec!["/nonexistent/anodizer/.env".to_string()]);
        let env = crate::MapEnvSource::new().with("HOME", home);
        assert!(
            token_file_tokens(Some(&listed), &env).unwrap().is_empty(),
            "the list form of env_files reads no token file"
        );
    }

    fn write_env_file(dir: &std::path::Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn a_token_line_in_a_listed_env_file_is_read_for_its_forge_only() {
        use crate::scm::ScmTokenType::{GitHub, GitLab, Gitea};
        let dir = tempfile::TempDir::new().unwrap();
        let first = write_env_file(
            dir.path(),
            ".env.a",
            "GH_TOKEN=ghp_gh\nGITLAB_TOKEN=' glpat_dotenv '\nGITEA_TOKEN=\n",
        );
        let second = write_env_file(dir.path(), ".env.b", "GITHUB_TOKEN=ghp_dotenv\n");
        let listed = EnvFilesConfig::List(vec![first, second]);
        assert_eq!(
            env_file_token(Some(&listed), GitHub).unwrap().as_deref(),
            Some("ghp_dotenv"),
            "GITHUB_TOKEN comes before GH_TOKEN in the GitHub order"
        );
        assert_eq!(
            env_file_token(Some(&listed), GitLab).unwrap().as_deref(),
            Some("glpat_dotenv"),
            "the env-file value is trimmed"
        );
        assert_eq!(
            env_file_token(Some(&listed), Gitea).unwrap(),
            None,
            "a blank line is unset"
        );
        assert!(
            token_file_tokens(Some(&listed), &crate::MapEnvSource::new())
                .unwrap()
                .is_empty(),
            "an env-file line is not a token-file token, so it never picks the forge"
        );
    }

    #[test]
    fn only_the_list_form_has_env_file_tokens() {
        use crate::scm::ScmTokenType::GitHub;
        assert_eq!(env_file_token(None, GitHub).unwrap(), None);
        let mapped = EnvFilesConfig::TokenFiles(EnvFilesTokenConfig::default());
        assert_eq!(env_file_token(Some(&mapped), GitHub).unwrap(), None);
        let missing = EnvFilesConfig::List(vec!["/nonexistent/anodizer/.env".to_string()]);
        assert_eq!(
            env_file_token(Some(&missing), GitHub).unwrap(),
            None,
            "a missing env file is skipped"
        );
    }

    #[test]
    fn the_missing_token_hint_names_the_file_the_config_reads() {
        use crate::scm::ScmTokenType::{GitHub, GitLab, Gitea};
        let github = "set ANODIZER_GITHUB_TOKEN or GITHUB_TOKEN or GH_TOKEN, pass --token, or";
        assert_eq!(
            missing_token_hint(None, GitHub),
            format!("{github} write the token to ~/.config/anodizer/github_token")
        );
        let mapped = EnvFilesConfig::TokenFiles(EnvFilesTokenConfig {
            github_token: Some("~/custom/gh.txt".to_string()),
            ..Default::default()
        });
        assert_eq!(
            missing_token_hint(Some(&mapped), GitHub),
            format!("{github} write the token to ~/custom/gh.txt")
        );
        assert_eq!(
            missing_token_hint(Some(&mapped), GitLab),
            "set GITLAB_TOKEN, pass --token, or write the token to ~/.config/anodizer/gitlab_token",
            "an unset field names the default path"
        );
        let listed = EnvFilesConfig::List(vec![".env".to_string(), ".release.env".to_string()]);
        assert_eq!(
            missing_token_hint(Some(&listed), GitHub),
            format!("{github} put a GITHUB_TOKEN line in one of the env files: .env, .release.env")
        );
        assert_eq!(
            missing_token_hint(Some(&listed), Gitea),
            "set GITEA_TOKEN, pass --token, or put a GITEA_TOKEN line in one of the env files: \
             .env, .release.env"
        );
        assert_eq!(
            missing_token_hint(Some(&EnvFilesConfig::List(Vec::new())), GitLab),
            "set GITLAB_TOKEN or pass --token",
            "an empty list reads no file"
        );
    }

    #[test]
    fn unset_field_falls_back_to_goreleaser_when_anodizer_absent() {
        // Only the goreleaser path exists → the second candidate is used.
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path();
        let gore = home.join(".config/goreleaser/gitlab_token");
        std::fs::create_dir_all(gore.parent().unwrap()).unwrap();
        std::fs::write(&gore, "gl_fallback\n").unwrap();

        let env = crate::MapEnvSource::new().with("HOME", home.to_string_lossy().to_string());
        let vars =
            load_token_files_with_env(&EnvFilesTokenConfig::default(), &test_log(), &env).unwrap();
        assert_eq!(vars.get("GITLAB_TOKEN").unwrap(), "gl_fallback");
    }

    // load_env_files strict-mode error

    #[test]
    fn strict_mode_errors_on_missing_env_file() {
        let r = load_env_files(
            &["/nonexistent/anodizer/strict/.env".to_string()],
            &test_log(),
            true,
        );
        let err = r.unwrap_err();
        assert!(
            err.contains("strict mode"),
            "expected strict-mode error, got: {err}"
        );
    }
}
