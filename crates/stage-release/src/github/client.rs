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
    // The non-`skip_tls` branch builds its `hyper_rustls` connector without an
    // explicit provider, so it resolves the process-default rustls
    // `CryptoProvider`. The dependency graph links both `ring` and `aws-lc-rs`
    // (object_store/reqwest pull the latter under rustls 0.23), so rustls
    // refuses to auto-select and panics unless a default is installed. `main()`
    // installs `ring` at startup, but this constructor must not depend on a
    // particular binary entry point having run — unit tests build it directly
    // under nextest's process-per-test model. Install up front; idempotent.
    anodizer_core::tls::install_default_crypto_provider();

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

#[cfg(test)]
mod tests {
    //! End-to-end proof that the `RetryAfterLayer` actually captures the
    //! server's `Retry-After` header through the FULL production middleware
    //! stack built by [`build_octocrab_client`] — not just by writing the
    //! capture directly (which the `secondary_rate_limit.rs` unit tests do).
    //!
    //! This is the test the layer-order comment in `build_octocrab_client`
    //! depends on: any reordering that moved `RetryAfterLayer` outside
    //! octocrab's error-mapping (which strips response headers) would make
    //! the capture silently read 0, and this test would catch it.
    use super::*;
    use anodizer_core::config::GitHubUrlsConfig;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
    use std::time::Duration;

    #[tokio::test]
    async fn retry_after_header_is_captured_through_the_layer_stack() {
        // A 403 secondary-RL response carrying `Retry-After: 90`. octocrab's
        // error mapping discards the header when it builds the typed error, so
        // the only way `capture` ends up non-zero is the `RetryAfterLayer`
        // reading it off the raw response first.
        let body = r#"{"message":"You have exceeded a secondary rate limit","documentation_url":"https://docs.github.com/rest/overview/resources-in-the-rest-api#secondary-rate-limits"}"#;
        let resp = Box::leak(
            format!(
                "HTTP/1.1 403 Forbidden\r\n\
                 Content-Type: application/json\r\n\
                 Retry-After: 90\r\n\
                 Content-Length: {}\r\n\
                 \r\n\
                 {body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);

        let github_urls = Some(GitHubUrlsConfig {
            api: Some(format!("http://{addr}/")),
            upload: Some(format!("http://{addr}/")),
            download: Some(format!("http://{addr}/")),
            skip_tls_verify: None,
        });
        let (octo, capture) =
            build_octocrab_client("test-token", &github_urls).expect("build client");

        assert!(
            capture.get().is_none(),
            "capture must start empty (no response seen yet)"
        );

        // The request errors (403), but the layer must have captured the
        // header before octocrab stripped it.
        let _ = octo
            .get::<serde_json::Value, _, _>("/test", None::<&()>)
            .await
            .expect_err("403 must surface as an error");

        assert_eq!(
            capture.get(),
            Some(Duration::from_secs(90)),
            "RetryAfterLayer must capture `Retry-After: 90` through the full stack"
        );
    }
}
