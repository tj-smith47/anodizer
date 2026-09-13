//! OCI registry manifest probe for the docker landing check.
//!
//! A `docker buildx build --push` (or `docker manifest push`) returning OK
//! proves the client finished its upload — not that the registry serves the
//! tag afterwards. A registry garbage-collecting a half-written upload, a
//! proxy that accepted and dropped it, or a retention policy that reclaimed
//! the tag all leave a release whose images cannot be pulled.
//!
//! So the probe asks the registry the same question a `docker pull` asks:
//! `GET /v2/<repository>/manifests/<reference>` over the distribution API,
//! with the standard `WWW-Authenticate: Bearer` token exchange when the
//! registry demands one. No docker daemon is involved, so the gate works on a
//! runner that built nothing.
//!
//! The verdict carries the CONTENT DIGEST the reference resolves to. Every
//! conformant registry names it in the `Docker-Content-Digest` response
//! header; where one does not, the SHA-256 over the served bytes answers the
//! same question, which is what a content digest is defined to be (the
//! `Accept` header below asks for every media type a push can produce, so no
//! registry downgrades the document into different bytes).

use anodizer_core::hashing::hex_lower;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::{
    RetryLog, RetryPolicy, SuccessClass, http_status, retry_http_blocking_bytes_deadline,
};
use anyhow::{Context as _, Result};
use sha2::{Digest, Sha256};

/// Every manifest media type an anodizer push can produce. A registry serves
/// the stored document unconverted only when the request accepts its media
/// type; a narrower `Accept` invites a schema downgrade whose bytes — and
/// therefore whose digest — differ from what was pushed.
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.docker.distribution.manifest.v2+json";

/// The registry a reference with no host component belongs to.
const DEFAULT_REGISTRY: &str = "docker.io";

/// Docker Hub's distribution API endpoint. `docker.io` is the name references
/// spell; it is not the host that answers `/v2/`.
const DOCKER_HUB_ENDPOINT: &str = "registry-1.docker.io";

/// The namespace a bare Docker Hub name (`alpine`) lives in.
const DOCKER_HUB_DEFAULT_NAMESPACE: &str = "library";

/// The tag a reference with neither tag nor digest means.
const DEFAULT_TAG: &str = "latest";

/// Probe timeout per request — a manifest is a small JSON document, and the
/// token exchange in front of it is smaller still.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A docker image reference split into the parts the distribution API needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageRef {
    /// The registry as the reference spells it (`ghcr.io`, `docker.io`).
    pub(crate) registry: String,
    /// The repository path (`owner/app`, `library/alpine`).
    pub(crate) repository: String,
    /// The tag or digest the manifest is asked for by.
    pub(crate) reference: String,
}

impl ImageRef {
    /// The `/v2/` manifest URL for this reference.
    fn manifest_url(&self) -> String {
        let endpoint = if self.registry == DEFAULT_REGISTRY {
            DOCKER_HUB_ENDPOINT
        } else {
            &self.registry
        };
        // A loopback registry is served over plain HTTP — the same exemption
        // docker's own client makes, and what keeps this module's tests
        // offline.
        let scheme = if is_loopback(&self.registry) {
            "http"
        } else {
            "https"
        };
        format!(
            "{scheme}://{endpoint}/v2/{}/manifests/{}",
            self.repository, self.reference
        )
    }
}

/// Whether `host` (with an optional port) names the local machine.
///
/// An IPv6 literal is bracketed (`[::1]:5000`), so the port cannot be split
/// off at the first colon; and the whole `127.0.0.0/8` range is local, not
/// `127.0.0.1` alone — which is what docker's own insecure-by-default
/// exemption covers.
fn is_loopback(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(rest, |(h, _)| h),
        // An unbracketed host keeps every colon it has when more than one
        // remains: that is a bare IPv6 literal, not a host and a port.
        None => match host.rsplit_once(':') {
            Some((name, port))
                if !name.contains(':')
                    && !port.is_empty()
                    && port.chars().all(|c| c.is_ascii_digit()) =>
            {
                name
            }
            _ => host,
        },
    };
    name == "localhost"
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Split a docker image reference into registry, repository and tag/digest.
///
/// The grammar is the one docker itself applies: the first path component is
/// the registry only when it looks like a host (carries a dot, carries a port,
/// or is `localhost`); otherwise the reference belongs to Docker Hub, where a
/// name with no namespace lives under `library/`.
pub(crate) fn parse_image_ref(image: &str) -> Result<ImageRef> {
    let image = image.trim();
    anyhow::ensure!(!image.is_empty(), "empty image reference");
    let (registry, path) = match image.split_once('/') {
        Some((first, rest))
            if first.contains('.') || first.contains(':') || first == "localhost" =>
        {
            (first.to_string(), rest.to_string())
        }
        Some(_) => (DEFAULT_REGISTRY.to_string(), image.to_string()),
        None => (
            DEFAULT_REGISTRY.to_string(),
            format!("{DOCKER_HUB_DEFAULT_NAMESPACE}/{image}"),
        ),
    };
    let (repository, reference) = if let Some((repo, digest)) = path.split_once('@') {
        (repo.to_string(), digest.to_string())
    } else {
        match path.rfind(':') {
            // A colon before the last `/` is a registry port that belongs to
            // the repository path, not a tag separator.
            Some(idx) if !path[idx..].contains('/') => {
                (path[..idx].to_string(), path[idx + 1..].to_string())
            }
            _ => (path.clone(), DEFAULT_TAG.to_string()),
        }
    };
    anyhow::ensure!(
        !repository.is_empty() && !reference.is_empty(),
        "unparseable image reference '{image}'"
    );
    Ok(ImageRef {
        registry,
        repository,
        reference,
    })
}

/// The registry an image reference names, for status wording.
pub(crate) fn image_registry(image: &str) -> String {
    parse_image_ref(image)
        .map(|r| r.registry)
        .unwrap_or_else(|_| DEFAULT_REGISTRY.to_string())
}

/// The content digest the registry serves for `image`, or `None` when the
/// registry answers that the reference does not exist.
///
/// `Err` means the registry could not be consulted — an unreachable host, a
/// credential the registry rejected — which the caller must report as
/// unverifiable rather than as an absence: a pushed tag that a 401 hid is
/// still live for everyone holding a pull credential.
pub fn manifest_digest(
    image: &str,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    log: &StageLogger,
) -> Result<Option<String>> {
    let parsed = parse_image_ref(image)?;
    let client = anodizer_core::http::blocking_client(PROBE_TIMEOUT)
        .context("build HTTP client for registry manifest probe")?;
    manifest_digest_with(&client, &parsed, policy, deadline, log)
}

/// [`manifest_digest`] against an explicit client and parsed reference, which
/// is what this module's tests drive against a loopback responder.
fn manifest_digest_with(
    client: &reqwest::blocking::Client,
    parsed: &ImageRef,
    policy: &RetryPolicy,
    deadline: Option<std::time::Instant>,
    log: &StageLogger,
) -> Result<Option<String>> {
    let url = parsed.manifest_url();
    let label = format!(
        "verify-release: query {} for '{}:{}'",
        parsed.registry, parsed.repository, parsed.reference
    );
    // What the registry itself called the document it served, kept from the
    // attempt that answered.
    let named_digest: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
    let send = |_attempt: u32| {
        let ask = || {
            client
                .get(&url)
                .header(reqwest::header::ACCEPT, MANIFEST_ACCEPT)
        };
        let resp = ask().send()?;
        let resp = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            answer_challenge(client, &parsed.registry, resp, ask)?
        } else {
            resp
        };
        *named_digest.borrow_mut() = content_digest_header(&resp);
        Ok(resp)
    };
    match retry_http_blocking_bytes_deadline(
        RetryLog::new(&label, log),
        policy,
        deadline,
        SuccessClass::Strict,
        send,
        |status, body| {
            format!(
                "registry returned {status} for '{}:{}': {body}",
                parsed.repository, parsed.reference
            )
        },
    ) {
        // The body is read as raw bytes, never as text: a lossy decode
        // substitutes U+FFFD for anything it cannot read and would hash to a
        // digest nothing serves, in the one code path whose whole job is
        // digest correctness.
        Ok((_status, body)) => Ok(Some(
            named_digest.take().unwrap_or_else(|| content_digest(&body)),
        )),
        Err(err) if http_status(&err) == 404 => Ok(None),
        Err(err) => Err(err),
    }
}

/// Answer a registry's `WWW-Authenticate` challenge and re-ask, or hand the
/// 401 back when the challenge names nothing this probe can answer.
///
/// The scheme decides how: a `Bearer` realm is exchanged for a token, a
/// `Basic` challenge wants the stored credential itself. Handing a `Basic`
/// challenge to the token exchange asks a realm that is a human-readable
/// name rather than a URL — which is how a stock `registry:2` behind htpasswd
/// answers — so the scheme is read first.
fn answer_challenge<F>(
    client: &reqwest::blocking::Client,
    registry: &str,
    unauthorized: reqwest::blocking::Response,
    ask: F,
) -> Result<reqwest::blocking::Response, reqwest::Error>
where
    F: Fn() -> reqwest::blocking::RequestBuilder,
{
    let challenge = unauthorized
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let (scheme, params) = challenge
        .split_once(char::is_whitespace)
        .unwrap_or((challenge.as_str(), ""));
    match scheme.to_ascii_lowercase().as_str() {
        "bearer" => match bearer_token(client, params, registry)? {
            Some(token) => ask().bearer_auth(token).send(),
            None => Ok(unauthorized),
        },
        "basic" => match stored_registry_auth(registry) {
            Some(basic) => ask()
                .header(reqwest::header::AUTHORIZATION, format!("Basic {basic}"))
                .send(),
            None => Ok(unauthorized),
        },
        _ => Ok(unauthorized),
    }
}

/// The digest the registry itself names for the document it served.
///
/// `Docker-Content-Digest` is what a `docker pull` resolves the reference to,
/// so it answers the landing question directly and without re-deriving it
/// from a body the registry might have served in another media type.
fn content_digest_header(resp: &reqwest::blocking::Response) -> Option<String> {
    let value = resp.headers().get("docker-content-digest")?.to_str().ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The `sha256:<hex>` content digest of a served manifest.
fn content_digest(body: &[u8]) -> String {
    format!("sha256:{}", hex_lower(&Sha256::digest(body)))
}

/// Whether two digest spellings name the same content. The pushed value may
/// arrive without the algorithm prefix and in either case, so both sides are
/// normalised before comparison.
pub(crate) fn digests_match(expected: &str, served: &str) -> bool {
    fn normalise(d: &str) -> String {
        d.trim()
            .trim_start_matches("sha256:")
            .to_ascii_lowercase()
            .to_string()
    }
    !expected.trim().is_empty() && normalise(expected) == normalise(served)
}

/// Exchange a registry's `WWW-Authenticate: Bearer` challenge for a token.
///
/// The token endpoint is asked with the credential the PUSH used — the entry
/// docker wrote into its own config for this registry — so a private
/// repository is probed as its publisher, and a public one anonymously when
/// no credential is stored. `None` means the challenge named no realm, or the
/// endpoint returned no token; the caller then reports the registry's own
/// 401.
fn bearer_token(
    client: &reqwest::blocking::Client,
    challenge: &str,
    registry: &str,
) -> Result<Option<String>, reqwest::Error> {
    let Some(realm) = challenge_param(challenge, "realm") else {
        return Ok(None);
    };
    let mut req = client.get(&realm);
    if let Some(service) = challenge_param(challenge, "service") {
        req = req.query(&[("service", service)]);
    }
    // A registry may name one scope per resource, and the token endpoint
    // reads them as repeated parameters; keeping only the first asks for a
    // token that covers one of them.
    for scope in challenge_params(challenge, "scope") {
        req = req.query(&[("scope", scope)]);
    }
    if let Some(basic) = stored_registry_auth(registry) {
        // The stored value is already the base64 `user:password` payload
        // docker keeps, so it is forwarded verbatim rather than decoded and
        // re-encoded.
        req = req.header(reqwest::header::AUTHORIZATION, format!("Basic {basic}"));
    }
    let resp = req.send()?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let body = resp.text()?;
    let parsed: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    Ok(["token", "access_token"]
        .iter()
        .find_map(|k| parsed.get(*k).and_then(|v| v.as_str()))
        .map(str::to_string))
}

/// The first value a `WWW-Authenticate` challenge gives for `key`.
fn challenge_param(challenge: &str, key: &str) -> Option<String> {
    challenge_params(challenge, key).into_iter().next()
}

/// Every value a `WWW-Authenticate` challenge gives for `key`, in order.
///
/// Parameters are comma-separated and their values are quoted or bare, so the
/// split respects quotes and the quotes are then stripped. Keys match whole —
/// a substring search for `realm="` also matches an `xrealm="` nobody asked
/// for — and the auth-scheme token in front of the first parameter is
/// ignored, so the whole header value or its parameter part both parse.
fn challenge_params(challenge: &str, key: &str) -> Vec<String> {
    let mut chunks: Vec<&str> = Vec::new();
    let mut quoted = false;
    let mut start = 0usize;
    for (idx, ch) in challenge.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                chunks.push(&challenge[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    chunks.push(&challenge[start..]);
    chunks
        .into_iter()
        .filter_map(|chunk| {
            let (name, value) = chunk.split_once('=')?;
            // `Bearer realm="…"` puts the scheme ahead of the first key.
            let name = name.trim().rsplit(char::is_whitespace).next()?;
            if !name.eq_ignore_ascii_case(key) {
                return None;
            }
            let value = value.trim().trim_matches('"').trim();
            (!value.is_empty()).then(|| value.to_string())
        })
        .collect()
}

/// The base64 `user:password` payload docker stored for `registry`, when it
/// stored one as a plain entry.
///
/// A credential kept by an external helper (`credsStore` / `credHelpers`) is
/// not read: reading it means running the helper binary, and the probe
/// degrades to anonymous instead, which reaches every public repository.
fn stored_registry_auth(registry: &str) -> Option<String> {
    let dir = match std::env::var("DOCKER_CONFIG") {
        Ok(d) if !d.is_empty() => std::path::PathBuf::from(d),
        _ => dirs_home()?.join(".docker"),
    };
    let raw = std::fs::read_to_string(dir.join("config.json")).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let auths = doc.get("auths")?.as_object()?;
    stored_auth_for(auths, registry)
}

/// Find `registry`'s entry in a docker config `auths` map, in every spelling
/// docker writes: the bare host, a scheme-qualified URL, and — for Docker Hub
/// — the legacy `index.docker.io/v1/` key that `docker login` still writes.
fn stored_auth_for(
    auths: &serde_json::Map<String, serde_json::Value>,
    registry: &str,
) -> Option<String> {
    let mut keys = vec![
        registry.to_string(),
        format!("https://{registry}"),
        format!("https://{registry}/"),
        format!("http://{registry}"),
    ];
    if registry == DEFAULT_REGISTRY {
        keys.push("https://index.docker.io/v1/".to_string());
        keys.push("index.docker.io".to_string());
    }
    keys.iter()
        .find_map(|k| auths.get(k))
        .and_then(|e| e.get("auth"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The user's home directory, for the default docker config location.
fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests;
