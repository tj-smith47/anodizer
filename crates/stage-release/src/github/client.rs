use std::sync::Arc;

use anodizer_core::config::GitHubUrlsConfig;
use anyhow::{Context as _, Result};
use http::header::HeaderValue;
use octocrab::service::middleware::auth_header::AuthHeaderLayer;
use octocrab::service::middleware::base_uri::BaseUriLayer;
use octocrab::service::middleware::extra_headers::ExtraHeadersLayer;

use super::secondary_rate_limit::{RetryAfterCapture, RetryAfterLayer};
use crate::release_log;

/// Build an octocrab client, optionally configured for GitHub Enterprise.
///
/// Returns the client together with a [`RetryAfterCapture`] whose tower
/// middleware layer intercepts every HTTP response and stores the server's
/// `Retry-After` header value (integer seconds) so the retry loops can
/// honour it instead of always falling back to a constant.
///
/// When `github_urls` is `None` or has no custom API URL, this produces a
/// standard GitHub.com client.  When an `api` URL is set, the octocrab
/// builder's `base_uri` is pointed at the Enterprise API endpoint.  If
/// `upload` is set, `upload_uri` is also overridden (octocrab uses this for
/// release asset uploads).
///
/// Both the normal and `skip_tls_verify` paths use the manual
/// `OctocrabBuilder::with_service` / `with_layer` construction so the
/// `RetryAfterLayer` can be injected into the middleware stack before
/// octocrab's error mapping discards response headers.
pub(crate) fn build_octocrab_client(
    token: &str,
    github_urls: &Option<GitHubUrlsConfig>,
) -> Result<(octocrab::Octocrab, RetryAfterCapture)> {
    let skip_tls = github_urls
        .as_ref()
        .and_then(|u| u.skip_tls_verify)
        .unwrap_or(false);

    let capture = RetryAfterCapture::new();

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

    let auth_header: HeaderValue = format!("Bearer {}", token)
        .parse()
        .context("release: format auth header")?;

    let retry_layer = RetryAfterLayer::new(capture.clone());
    let headers_layer = ExtraHeadersLayer::new(Arc::new(vec![(
        http::header::USER_AGENT,
        HeaderValue::from_static("octocrab"),
    )]));
    let base_layer = BaseUriLayer::new(base_uri.clone());
    let auth_layer = AuthHeaderLayer::new(Some(auth_header), base_uri, upload_uri);

    // Layer order (innermost → outermost):
    //   hyper client → RetryAfter → ExtraHeaders → BaseUri → AuthHeader
    //
    // RetryAfterLayer sits closest to the transport so it sees the raw HTTP
    // response (with all headers) before any upper layer processes or
    // transforms it. octocrab's error mapping runs after all layers, so by
    // the time it strips headers the capture has already stored the value.
    //
    // Both branches duplicate the 7-line builder chain because the hyper
    // client types differ (TimeoutConnector vs plain HttpsConnector) and
    // naming the body type (`octocrab::body::OctoBody`) is impossible from
    // outside the crate. Inlining lets type inference resolve it.

    let octo = if skip_tls {
        release_log()
            .warn("TLS certificate verification disabled for GitHub API — this is insecure");

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

        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(connector);

        octocrab::OctocrabBuilder::new_empty()
            .with_service(client)
            .with_layer(&retry_layer)
            .with_layer(&headers_layer)
            .with_layer(&base_layer)
            .with_layer(&auth_layer)
            .with_auth(octocrab::AuthState::None)
            .build()
            .map_err(|e| match e {})?
    } else {
        // Explicit timeouts are essential for release-asset uploads: without
        // them a stalled connection to uploads.github.com can hang for
        // minutes until TCP keepalive gives up.
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("release: load native TLS root certificates")
            .https_or_http()
            .enable_http1()
            .build();

        let mut timeout = hyper_timeout::TimeoutConnector::new(connector);
        timeout.set_connect_timeout(Some(std::time::Duration::from_secs(30)));
        timeout.set_read_timeout(Some(std::time::Duration::from_secs(120)));
        timeout.set_write_timeout(Some(std::time::Duration::from_secs(120)));

        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(timeout);

        octocrab::OctocrabBuilder::new_empty()
            .with_service(client)
            .with_layer(&retry_layer)
            .with_layer(&headers_layer)
            .with_layer(&base_layer)
            .with_layer(&auth_layer)
            .with_auth(octocrab::AuthState::None)
            .build()
            .map_err(|e| match e {})?
    };

    Ok((octo, capture))
}

/// A [`rustls::client::danger::ServerCertVerifier`] that accepts all certificates
/// unconditionally.  Used only when `github_urls.skip_tls_verify` is explicitly
/// enabled — typically for self-signed GitHub Enterprise instances in development
/// or air-gapped environments.
#[derive(Debug)]
struct DangerousNoCertVerifier {
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
