use super::*;

/// Build a [`reqwest::Client`] configured for GitLab API access.
///
/// - `token`: the GITLAB_TOKEN or CI_JOB_TOKEN value.
/// - `skip_tls_verify`: when true, disable TLS certificate verification.
/// - `use_job_token`: when true, use `JOB-TOKEN` header instead of `PRIVATE-TOKEN`.
///
/// The token is set as a default header on all requests from the returned client.
pub(crate) fn build_gitlab_client(
    token: &str,
    skip_tls_verify: bool,
    use_job_token: bool,
) -> Result<Client> {
    gitlab_client_builder(token, skip_tls_verify, use_job_token)?
        .build()
        .context("gitlab: build HTTP client")
}

/// Like [`build_gitlab_client`], but with redirect-following disabled — for
/// the best-effort HEAD size probe on release-link URLs.
///
/// The client's default headers carry the PRIVATE-TOKEN / JOB-TOKEN on every
/// request, and reqwest strips only `Authorization`/`Cookie` on a cross-host
/// redirect — a custom auth header follows it. GitLab object storage with
/// `proxy_download` off answers the link URL with a 302 to a pre-signed
/// external store, so a redirect-following probe would hand the token to
/// that host. With redirects off the 302 is a non-success → the probe
/// degrades to "size unknown", which can never mis-skip an upload.
pub(crate) fn build_gitlab_probe_client(
    token: &str,
    skip_tls_verify: bool,
    use_job_token: bool,
) -> Result<Client> {
    gitlab_client_builder(token, skip_tls_verify, use_job_token)?
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("gitlab: build HTTP probe client")
}

/// Shared builder for [`build_gitlab_client`] / [`build_gitlab_probe_client`]
/// so the auth-header / TLS / timeout policy cannot drift between the two.
fn gitlab_client_builder(
    token: &str,
    skip_tls_verify: bool,
    use_job_token: bool,
) -> Result<reqwest::ClientBuilder> {
    let header_name = auth_header(use_job_token);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_bytes(header_name.as_bytes())
            .context("gitlab: invalid auth header name")?,
        reqwest::header::HeaderValue::from_str(token)
            .context("gitlab: invalid token value for header")?,
    );

    Ok(Client::builder()
        .default_headers(headers)
        .danger_accept_invalid_certs(skip_tls_verify)
        .timeout(std::time::Duration::from_secs(300)))
}
