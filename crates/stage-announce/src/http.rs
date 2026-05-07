use anyhow::{Context as _, Result};

/// POST a JSON payload to `url`, returning an error that includes the
/// provider name, HTTP status, and response body on failure.
///
/// The URL is intentionally NOT included in error messages because it may
/// contain secrets (e.g. Telegram bot tokens embedded in the path).
pub(crate) fn post_json(url: &str, payload: &str, provider: &str) -> Result<()> {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(payload.to_string())
        .send()
        .with_context(|| format!("{}: failed to send POST request", provider))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = anodizer_core::http::body_of_blocking(resp);
        anyhow::bail!("{}: HTTP {} — {}", provider, status, body);
    }
    Ok(())
}
