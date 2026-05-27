//! OpenAI Chat Completions API provider for AI changelog enhancement.

use std::time::Duration;

use anodizer_core::http::blocking_client;
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use super::AiProvider;

/// Default model for the OpenAI provider.
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// HTTP request timeout for OpenAI API calls.
const TIMEOUT: Duration = Duration::from_secs(120);

/// OpenAI Chat Completions API provider.
///
/// Reads auth from `OPENAI_API_KEY`. Endpoint:
/// `https://api.openai.com/v1/chat/completions`. Default model: `gpt-4o-mini`.
pub struct OpenAiProvider {
    /// Base URL (overridable for tests via `ANODIZER_OPENAI_API_BASE`).
    base_url: String,
}

impl OpenAiProvider {
    /// Construct from environment. Reads `ANODIZER_OPENAI_API_BASE` for
    /// test overrides; production callers get `https://api.openai.com`.
    pub fn from_env() -> Self {
        let base_url = std::env::var("ANODIZER_OPENAI_API_BASE")
            .unwrap_or_else(|_| "https://api.openai.com".to_string());
        Self { base_url }
    }
}

impl AiProvider for OpenAiProvider {
    fn enhance(&self, prompt: &str, model: Option<&str>) -> Result<String> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .context("OPENAI_API_KEY is not set; required for the openai provider")?;
        if api_key.is_empty() {
            bail!("OPENAI_API_KEY is empty; required for the openai provider");
        }

        let model = model.unwrap_or(DEFAULT_MODEL);
        let body = json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}]
        });

        let client = blocking_client(TIMEOUT).context("openai: build HTTP client")?;
        let url = format!("{}/v1/chat/completions", self.base_url);
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .context("openai: POST /v1/chat/completions")?;

        let status = resp.status();
        let text = resp.text().unwrap_or_default();

        if !status.is_success() {
            bail!("openai: request failed ({status}): {text}");
        }

        let parsed: Value = serde_json::from_str(&text).context("openai: parse response JSON")?;

        // Extract choices[0].message.content.
        parsed["choices"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|choice| choice["message"]["content"].as_str().map(str::to_owned))
            .ok_or_else(|| anyhow::anyhow!("openai: no content in choices[0].message: {text}"))
    }

    fn default_model(&self) -> &str {
        DEFAULT_MODEL
    }
}
