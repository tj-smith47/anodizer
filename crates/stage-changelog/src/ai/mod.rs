//! AI-powered changelog enhancement.
//!
//! Providers implement [`AiProvider`]. The orchestration entry point is
//! [`enhance_with_ai`], which resolves the prompt source, renders it through
//! Tera (injecting `.ReleaseNotes` scoped to this call), dispatches to the
//! configured provider, and returns the enhanced body.

mod anthropic;
mod ollama;
mod openai;

#[cfg(test)]
mod tests;

use anodizer_core::config::{ChangelogAiConfig, ChangelogAiPrompt};
use anodizer_core::context::Context;
use anodizer_core::env_expand::expand_env;
use anodizer_core::http::blocking_client;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};

pub use anthropic::AnthropicProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAiProvider;

// ---------------------------------------------------------------------------
// Default prompt
// ---------------------------------------------------------------------------

/// Default AI prompt used when `changelog.ai.prompt` is not configured.
///
/// Derived from GoReleaser's canonical gist (frozen copy; matches the
/// behaviour documented in changelog.md line 232). Asks the model to:
/// - Write a short intro paragraph.
/// - Merge dependency-bump commits into a single "Dependencies updated" line.
/// - Omit emojis.
const DEFAULT_PROMPT: &str = r#"You are a technical writer for a software project.
You will be given a changelog for a new release.
Please write a short and concise description for the release based on the changelog.
The description should be written in markdown format.
Please do NOT include any emojis in the response.
Please group all dependency updates into a single item called "Dependencies updated".
Finally, add the changelog to the end of the description.

Here's the changelog:

{{ ReleaseNotes }}"#;

// ---------------------------------------------------------------------------
// AiProvider trait
// ---------------------------------------------------------------------------

/// A pluggable AI provider for changelog enhancement.
///
/// Each impl handles auth, endpoint selection, request serialization, and
/// response extraction for one backend (Anthropic, OpenAI, Ollama).
pub trait AiProvider {
    /// Send `prompt` (which already contains the rendered release notes) to
    /// the provider and return the enhanced text.
    ///
    /// `model` overrides the provider's default when `Some`.
    fn enhance(&self, prompt: &str, model: Option<&str>) -> Result<String>;

    /// Provider's built-in default model name, used when `model` is `None`.
    fn default_model(&self) -> &str;
}

// ---------------------------------------------------------------------------
// Provider dispatch
// ---------------------------------------------------------------------------

/// Construct the appropriate [`AiProvider`] for `provider_name`.
///
/// Returns an error with a helpful list of valid names on an unrecognised value.
fn make_provider(provider_name: &str) -> Result<Box<dyn AiProvider>> {
    match provider_name {
        "anthropic" => Ok(Box::new(AnthropicProvider::from_env())),
        "openai" => Ok(Box::new(OpenAiProvider::from_env())),
        "ollama" => Ok(Box::new(OllamaProvider::from_env())),
        other => bail!(
            "changelog.ai: unknown provider {:?} (valid: anthropic, openai, ollama)",
            other
        ),
    }
}

// ---------------------------------------------------------------------------
// Prompt resolution
// ---------------------------------------------------------------------------

/// Fetch the raw prompt text from the configured source.
///
/// Priority: `from_file` > `from_url` > inline string > default prompt.
/// Header values in `from_url` are `${VAR}` / `$VAR` expanded from the
/// process environment before the request is sent.
fn resolve_raw_prompt(cfg: &ChangelogAiConfig) -> Result<String> {
    let Some(ref prompt_cfg) = cfg.prompt else {
        return Ok(DEFAULT_PROMPT.to_owned());
    };

    match prompt_cfg {
        ChangelogAiPrompt::Inline(s) => {
            if s.trim().is_empty() {
                Ok(DEFAULT_PROMPT.to_owned())
            } else {
                Ok(s.clone())
            }
        }
        ChangelogAiPrompt::Source(src) => {
            // from_file takes priority over from_url.
            if let Some(ref file_cfg) = src.from_file
                && let Some(ref path) = file_cfg.path
            {
                let content = std::fs::read_to_string(path)
                    .with_context(|| format!("changelog.ai: read prompt file {path:?}"))?;
                return Ok(content);
            }

            if let Some(ref url_cfg) = src.from_url
                && let Some(ref url) = url_cfg.url
            {
                let client = blocking_client(std::time::Duration::from_secs(30))
                    .context("changelog.ai: build HTTP client for prompt fetch")?;

                let mut req = client.get(url.as_str());

                // Expand ${VAR} / $VAR in header values before sending.
                if let Some(ref headers) = url_cfg.headers {
                    for (key, value) in headers {
                        req = req.header(key.as_str(), expand_env(value));
                    }
                }

                let resp = req
                    .send()
                    .with_context(|| format!("changelog.ai: GET prompt from {url}"))?;
                let status = resp.status();
                let text = resp.text().unwrap_or_default();
                if !status.is_success() {
                    bail!("changelog.ai: prompt URL {url} returned {status}: {text}");
                }
                return Ok(text);
            }

            // Source configured but neither path nor url resolved — use default.
            Ok(DEFAULT_PROMPT.to_owned())
        }
    }
}

// ---------------------------------------------------------------------------
// Tera render with injected ReleaseNotes var
// ---------------------------------------------------------------------------

/// Render `template` through Tera, injecting `ReleaseNotes = notes` into a
/// one-shot context so this var does NOT pollute the global template var table.
fn render_prompt(template: &str, notes: &str, ctx: &Context) -> Result<String> {
    // Clone the existing vars and inject ReleaseNotes as a structured value.
    let mut vars = ctx.template_vars().clone();
    vars.set_structured("ReleaseNotes", serde_json::Value::String(notes.to_owned()));
    anodizer_core::template::render(template, &vars).context("changelog.ai: render prompt template")
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Enhance `body` via the configured AI provider.
///
/// Called after the SCM changelog is rendered (or after `--release-notes-tmpl`
/// is applied — whichever produced `body`). Returns the provider's response
/// as the new body. On provider error the behaviour depends on
/// `ctx.options.allow_ai_failure`:
/// - `false` (default): propagate the error and abort the release.
/// - `true`: log a warning and return `body` unchanged.
pub fn enhance_with_ai(
    ctx: &Context,
    ai_cfg: &ChangelogAiConfig,
    body: &str,
    log: &StageLogger,
) -> Result<String> {
    let provider_name = match ai_cfg.provider.as_deref() {
        Some(p) if !p.is_empty() => p,
        _ => return Ok(body.to_owned()),
    };

    // Skip AI enhancement in snapshot mode — cost containment.
    if ctx.is_snapshot() {
        log.status("changelog.ai: skipped (snapshot mode)");
        return Ok(body.to_owned());
    }

    let raw_prompt = resolve_raw_prompt(ai_cfg).context("changelog.ai: resolve prompt")?;

    let rendered_prompt =
        render_prompt(&raw_prompt, body, ctx).context("changelog.ai: render prompt")?;

    let provider = make_provider(provider_name)?;

    log.status(&format!(
        "changelog.ai: enhancing release notes via {} (model: {})",
        provider_name,
        ai_cfg.model.as_deref().unwrap_or(provider.default_model())
    ));

    match provider.enhance(&rendered_prompt, ai_cfg.model.as_deref()) {
        Ok(enhanced) => Ok(enhanced),
        Err(err) => {
            if ctx.options.allow_ai_failure {
                log.warn(&format!(
                    "changelog.ai: provider error (--allow-ai-failure set, keeping original notes): {err:#}"
                ));
                Ok(body.to_owned())
            } else {
                Err(err.context(
                    "changelog.ai: provider failed (use --allow-ai-failure to degrade gracefully)",
                ))
            }
        }
    }
}
