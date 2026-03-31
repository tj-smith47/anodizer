use anyhow::Result;

/// Post a status (toot) to a Mastodon instance via the v1 statuses API.
///
/// `client_id` and `client_secret` are accepted for GoReleaser parity — some
/// Mastodon-compatible servers may require them for certain auth flows.
pub fn send_mastodon(
    server: &str,
    access_token: &str,
    client_id: &str,
    client_secret: &str,
    message: &str,
) -> Result<()> {
    let url = format!("{}/api/v1/statuses", server.trim_end_matches('/'));
    let client = reqwest::blocking::Client::new();

    let mut form = vec![("status", message.to_string())];
    if !client_id.is_empty() {
        form.push(("client_id", client_id.to_string()));
    }
    if !client_secret.is_empty() {
        form.push(("client_secret", client_secret.to_string()));
    }

    let resp = client
        .post(&url)
        .bearer_auth(access_token)
        .form(&form)
        .send()?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("mastodon: API request failed ({status}): {body}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {}
