//! Anthropic Messages API provider for AI changelog enhancement.

use std::time::Duration;

use anodizer_core::http::blocking_client;
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use super::AiProvider;

/// Default model for the Anthropic provider.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// HTTP request timeout for Anthropic API calls.
const TIMEOUT: Duration = Duration::from_secs(120);

/// Anthropic Messages API provider.
///
/// Reads auth from `ANTHROPIC_API_KEY`. Endpoint:
/// `https://api.anthropic.com/v1/messages` by default, overridable via
/// `ANODIZER_ANTHROPIC_ENDPOINT` to route through a corporate proxy,
/// regional mirror, or private gateway. Default model: `claude-sonnet-4-6`.
pub struct AnthropicProvider {
    /// Base URL for the Anthropic API (default `https://api.anthropic.com`).
    base_url: String,
}

impl AnthropicProvider {
    /// Construct from environment.
    ///
    /// `ANODIZER_ANTHROPIC_ENDPOINT` overrides the default
    /// `https://api.anthropic.com` base URL. Use this to point at a
    /// corporate proxy, regional mirror, or private gateway that
    /// re-exposes the Anthropic Messages API.
    pub fn from_env() -> Self {
        let base_url = std::env::var("ANODIZER_ANTHROPIC_ENDPOINT")
            .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
        Self { base_url }
    }
}

impl AiProvider for AnthropicProvider {
    fn enhance(&self, prompt: &str, model: Option<&str>) -> Result<String> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY is not set; required for the anthropic provider")?;
        if api_key.is_empty() {
            bail!("ANTHROPIC_API_KEY is empty; required for the anthropic provider");
        }

        let model = model.unwrap_or(DEFAULT_MODEL);
        let body = json!({
            "model": model,
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": prompt}]
        });

        let client = blocking_client(TIMEOUT).context("anthropic: build HTTP client")?;
        let url = format!("{}/v1/messages", self.base_url);
        let resp = client
            .post(&url)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .context("anthropic: POST /v1/messages")?;

        let status = resp.status();
        let text = resp.text().unwrap_or_default();

        if !status.is_success() {
            bail!("anthropic: request failed ({status}): {text}");
        }

        let parsed: Value =
            serde_json::from_str(&text).context("anthropic: parse response JSON")?;

        // Extract the first text block from the content array.
        parsed["content"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|block| {
                if block["type"].as_str() == Some("text") {
                    block["text"].as_str().map(str::to_owned)
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow::anyhow!("anthropic: no text block in response: {text}"))
    }

    fn default_model(&self) -> &str {
        DEFAULT_MODEL
    }
}
