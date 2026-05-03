use std::sync::Arc;

use anodizer_core::config::GitHubUrlsConfig;
use anyhow::{Context as _, Result};
use http::header::HeaderValue;
use octocrab::service::middleware::auth_header::AuthHeaderLayer;
use octocrab::service::middleware::base_uri::BaseUriLayer;
use octocrab::service::middleware::extra_headers::ExtraHeadersLayer;

use crate::release_log;

// ---------------------------------------------------------------------------
// build_octocrab_client — GitHub Enterprise URL support
// ---------------------------------------------------------------------------

/// Build an octocrab client, optionally configured for GitHub Enterprise.
///
/// When `github_urls` is `None` or has no custom API URL, this produces a
/// standard GitHub.com client.  When an `api` URL is set, the octocrab
/// builder's `base_uri` is pointed at the Enterprise API endpoint.  If
/// `upload` is set, `upload_uri` is also overridden (octocrab uses this for
/// release asset uploads).
///
/// `skip_tls_verify` is supported by constructing a custom `hyper_rustls`
/// connector whose `rustls::ClientConfig` disables certificate verification.
/// This is the same approach GoReleaser uses via Go's `InsecureSkipVerify`.
pub(crate) fn build_octocrab_client(
    token: &str,
    github_urls: &Option<GitHubUrlsConfig>,
) -> Result<octocrab::Octocrab> {
    let skip_tls = github_urls
        .as_ref()
        .and_then(|u| u.skip_tls_verify)
        .unwrap_or(false);

    if skip_tls {
        // Build a custom hyper client with TLS verification disabled, then
        // wrap it in octocrab's expected service layer stack.
        build_octocrab_client_insecure(token, github_urls)
    } else {
        // Normal path: use octocrab's built-in hyper client.
        //
        // Explicit timeouts are essential for release-asset uploads: without
        // them the hyper client has `None` for connect/read/write timeouts
        // (octocrab's defaults) and a stalled upload — common for 10+ MB
        // artifacts hitting a flaky edge on uploads.github.com — hangs for
        // minutes until TCP keepalive gives up, compounded 4× by octocrab's
        // default `RetryConfig::Simple(3)` middleware. A single stall can
        // consume 5–10 min before surfacing as `Error::Serde` (proxy HTML in
        // the body). Explicit idle timeouts surface the stall in ~60–90s so
        // our outer retry loop can take over cleanly.
        let mut builder = octocrab::Octocrab::builder()
            .personal_token(token.to_owned())
            .set_connect_timeout(Some(std::time::Duration::from_secs(30)))
            .set_read_timeout(Some(std::time::Duration::from_secs(120)))
            .set_write_timeout(Some(std::time::Duration::from_secs(120)));

        if let Some(urls) = github_urls {
            if let Some(api) = &urls.api {
                builder = builder
                    .base_uri(api.as_str())
                    .context("release: invalid github_urls.api URL")?;
            }
            if let Some(upload) = &urls.upload {
                builder = builder
                    .upload_uri(upload.as_str())
                    .context("release: invalid github_urls.upload URL")?;
            }
        }

        builder.build().context("release: build octocrab client")
    }
}

/// Build an octocrab client that skips TLS certificate verification.
///
/// This follows octocrab's `custom_client.rs` example pattern: construct a
/// hyper client with a custom `rustls::ClientConfig` that disables cert
/// verification, then wrap it in octocrab's middleware layers for auth, base
/// URI, and headers via `OctocrabBuilder::with_service` / `with_layer`.
fn build_octocrab_client_insecure(
    token: &str,
    github_urls: &Option<GitHubUrlsConfig>,
) -> Result<octocrab::Octocrab> {
    release_log().warn("TLS certificate verification disabled for GitHub API — this is insecure");

    // Build a rustls ClientConfig that accepts any server certificate.
    let crypto_provider = rustls::crypto::ring::default_provider();
    let tls_config = rustls::ClientConfig::builder_with_provider(Arc::new(crypto_provider))
        .with_safe_default_protocol_versions()
        .context("release: configure TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(DangerousNoCertVerifier::new()))
        .with_no_client_auth();

    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls_config)
        .https_or_http()
        .enable_http1()
        .build();

    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(connector);

    // Parse URIs the same way octocrab does.
    let base_uri: http::Uri = if let Some(api) = github_urls.as_ref().and_then(|u| u.api.as_ref()) {
        api.parse()
            .context("release: invalid github_urls.api URL")?
    } else {
        "https://api.github.com"
            .parse()
            .unwrap_or_else(|e| panic!("hardcoded URI is valid: {e}"))
    };

    let upload_uri: http::Uri =
        if let Some(upload) = github_urls.as_ref().and_then(|u| u.upload.as_ref()) {
            upload
                .parse()
                .context("release: invalid github_urls.upload URL")?
        } else {
            "https://uploads.github.com"
                .parse()
                .unwrap_or_else(|e| panic!("hardcoded URI is valid: {e}"))
        };

    // Follow octocrab's custom_client.rs example: with_service → with_layer
    // for BaseUri, ExtraHeaders, and AuthHeader, then with_auth → build.
    let auth_header: HeaderValue = format!("Bearer {}", token)
        .parse()
        .context("release: format auth header")?;

    octocrab::OctocrabBuilder::new_empty()
        .with_service(client)
        .with_layer(&ExtraHeadersLayer::new(Arc::new(vec![(
            http::header::USER_AGENT,
            HeaderValue::from_static("octocrab"),
        )])))
        .with_layer(&BaseUriLayer::new(base_uri.clone()))
        .with_layer(&AuthHeaderLayer::new(
            Some(auth_header),
            base_uri,
            upload_uri,
        ))
        .with_auth(octocrab::AuthState::None)
        .build()
        .map_err(|e| match e {}) // Infallible → never fails
}

/// A [`rustls::client::danger::ServerCertVerifier`] that accepts all certificates
/// unconditionally.  Used only when `github_urls.skip_tls_verify` is explicitly
/// enabled — typically for self-signed GitHub Enterprise instances in development
/// or air-gapped environments.
#[derive(Debug)]
struct DangerousNoCertVerifier {
    /// Pre-computed signature schemes from the ring crypto provider, avoiding
    /// a fresh `CryptoProvider` allocation on every call to `supported_verify_schemes`.
    schemes: Vec<rustls::SignatureScheme>,
}

impl DangerousNoCertVerifier {
    fn new() -> Self {
        Self {
            schemes: rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes(),
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for DangerousNoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.schemes.clone()
    }
}
