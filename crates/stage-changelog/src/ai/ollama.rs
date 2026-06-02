//! Ollama local inference provider for AI changelog enhancement.

use std::sync::Arc;
use std::time::Duration;

use anodizer_core::env_source::EnvSource;
use anyhow::Result;
use serde_json::json;

use super::{AiProvider, post_for_json};

/// Default model for the Ollama provider.
pub(crate) const DEFAULT_MODEL: &str = "llama3.1";

/// HTTP request timeout for Ollama API calls (longer to accommodate local inference).
const TIMEOUT: Duration = Duration::from_secs(300);

/// Ollama local inference provider.
///
/// No auth by default. Endpoint base from `ANODIZER_OLLAMA_ENDPOINT`,
/// then `OLLAMA_HOST` (the upstream Ollama convention), then defaults
/// to `http://localhost:11434`. Default model: `llama3.1`.
pub(crate) struct OllamaProvider {
    /// Base URL for the Ollama API.
    base_url: String,
}

impl OllamaProvider {
    /// Construct from the injected environment source.
    ///
    /// Precedence: `ANODIZER_OLLAMA_ENDPOINT` (anodizer-namespaced
    /// override for proxy / remote-Ollama setups) → `OLLAMA_HOST` (the
    /// upstream Ollama convention) → `http://localhost:11434`.
    pub(crate) fn from_env(env: Arc<dyn EnvSource>) -> Self {
        let base_url = env
            .var("ANODIZER_OLLAMA_ENDPOINT")
            .or_else(|| env.var("OLLAMA_HOST"))
            .unwrap_or_else(|| "http://localhost:11434".to_string());
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

        let url = format!("{}/api/generate", self.base_url);
        let parsed = post_for_json(
            TIMEOUT,
            &url,
            &[("content-type", "application/json".to_string())],
            &body,
            "ollama",
        )?;

        // Extract the "response" field.
        parsed["response"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("ollama: no `response` field in reply: {parsed}"))
    }

    fn default_model(&self) -> &str {
        DEFAULT_MODEL
    }
}
