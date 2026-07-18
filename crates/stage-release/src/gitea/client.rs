use super::*;

/// Build a [`reqwest::Client`] configured for Gitea API access.
///
/// - `token`: the GITEA_TOKEN value.
/// - `skip_tls_verify`: when true, disable TLS certificate verification.
///
/// Gitea uses `Authorization: token {value}` for all API requests.
pub(crate) fn build_gitea_client(token: &str, skip_tls_verify: bool) -> Result<Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("token {}", token))
            .context("gitea: invalid token value for Authorization header")?,
    );

    let builder = Client::builder()
        .default_headers(headers)
        .danger_accept_invalid_certs(skip_tls_verify)
        .timeout(std::time::Duration::from_secs(300));

    builder.build().context("gitea: build HTTP client")
}
