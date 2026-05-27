//! Ollama local inference provider for AI changelog enhancement.

use std::time::Duration;

use anodizer_core::http::blocking_client;
use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use super::AiProvider;

/// Default model for the Ollama provider.
pub const DEFAULT_MODEL: &str = "llama3.1";

/// HTTP request timeout for Ollama API calls (longer to accommodate local inference).
const TIMEOUT: Duration = Duration::from_secs(300);

/// Ollama local inference provider.
///
/// No auth by default. Endpoint base from `ANODIZER_OLLAMA_ENDPOINT`,
/// then `OLLAMA_HOST` (the upstream Ollama convention), then defaults
/// to `http://localhost:11434`. Default model: `llama3.1`.
pub struct OllamaProvider {
    /// Base URL for the Ollama API.
    base_url: String,
}

impl OllamaProvider {
    /// Construct from environment.
    ///
    /// Precedence: `ANODIZER_OLLAMA_ENDPOINT` (anodizer-namespaced
    /// override for proxy / remote-Ollama setups) → `OLLAMA_HOST` (the
    /// upstream Ollama convention) → `http://localhost:11434`.
    pub fn from_env() -> Self {
        let base_url = std::env::var("ANODIZER_OLLAMA_ENDPOINT")
            .or_else(|_| std::env::var("OLLAMA_HOST"))
            .unwrap_or_else(|_| "http://localhost:11434".to_string());
        Self { base_url }
    }
}

impl AiProvider for OllamaProvider {
    fn enhance(&self, prompt: &str, model: Option<&str>) -> Result<String> {
        let model = model.unwrap_or(DEFAULT_MODEL);
        let body = json!({
            "model": model,
            "prompt": prompt,
            "stream": false
        });

        let client = blocking_client(TIMEOUT).context("ollama: build HTTP client")?;
        let url = format!("{}/api/generate", self.base_url);
        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .context("ollama: POST /api/generate")?;

        let status = resp.status();
        let text = resp.text().unwrap_or_default();

        if !status.is_success() {
            anyhow::bail!("ollama: request failed ({status}): {text}");
        }

        let parsed: Value = serde_json::from_str(&text).context("ollama: parse response JSON")?;

        // Extract the "response" field.
        parsed["response"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("ollama: no `response` field in reply: {text}"))
    }

    fn default_model(&self) -> &str {
        DEFAULT_MODEL
    }
}
