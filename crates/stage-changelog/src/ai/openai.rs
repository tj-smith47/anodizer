//! OpenAI Chat Completions API provider for AI changelog enhancement.

use std::sync::Arc;
use std::time::Duration;

use anodizer_core::env_source::EnvSource;
use anyhow::Result;
use serde_json::json;

use super::{AiProvider, post_for_json};

/// Default model for the OpenAI provider.
pub(crate) const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// HTTP request timeout for OpenAI API calls.
const TIMEOUT: Duration = Duration::from_secs(120);

/// OpenAI Chat Completions API provider.
///
/// Reads auth from `OPENAI_API_KEY`. Endpoint:
/// `https://api.openai.com/v1/chat/completions` by default, overridable
/// via `ANODIZER_OPENAI_ENDPOINT` to route through a corporate proxy,
/// Azure OpenAI gateway, or any OpenAI-compatible inference server.
/// Default model: `gpt-4o-mini`.
pub(crate) struct OpenAiProvider {
    /// Base URL for the OpenAI API (default `https://api.openai.com`).
    base_url: String,
    /// Injected environment-variable source used for the API key lookup
    /// at `enhance` time. See [`AnthropicProvider`](super::AnthropicProvider)
    /// for the rationale.
    env: Arc<dyn EnvSource>,
}

impl OpenAiProvider {
    /// Construct from the injected environment source.
    ///
    /// `ANODIZER_OPENAI_ENDPOINT` overrides the default
    /// `https://api.openai.com` base URL. Use this to point at a
    /// corporate proxy, an Azure OpenAI gateway, or any OpenAI-API-
    /// compatible inference server.
    pub(crate) fn from_env(env: Arc<dyn EnvSource>) -> Self {
        let base_url = env
            .var("ANODIZER_OPENAI_ENDPOINT")
            .unwrap_or_else(|| "https://api.openai.com".to_string());
        Self { base_url, env }
    }
}

impl AiProvider for OpenAiProvider {
    fn enhance(&self, prompt: &str, model: Option<&str>) -> Result<String> {
        let api_key = self
            .env
            .var("OPENAI_API_KEY")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("OPENAI_API_KEY is not set; required for the openai provider")
            })?;

        let model = model.unwrap_or(DEFAULT_MODEL);
        let body = json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}]
        });

        let url = format!("{}/v1/chat/completions", self.base_url);
        let parsed = post_for_json(
            TIMEOUT,
            &url,
            &[
                ("Authorization", format!("Bearer {api_key}")),
                ("content-type", "application/json".to_string()),
            ],
            &body,
            "openai",
        )?;

        // Extract choices[0].message.content.
        parsed["choices"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|choice| choice["message"]["content"].as_str().map(str::to_owned))
            .ok_or_else(|| anyhow::anyhow!("openai: no content in choices[0].message: {parsed}"))
    }

    fn default_model(&self) -> &str {
        DEFAULT_MODEL
    }
}
