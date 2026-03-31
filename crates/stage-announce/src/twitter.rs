use anyhow::Result;
use base64::Engine;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Post a tweet via Twitter API v2 with OAuth 1.0a user-context authentication.
pub fn send_twitter(
    consumer_key: &str,
    consumer_secret: &str,
    access_token: &str,
    access_token_secret: &str,
    message: &str,
) -> Result<()> {
    let url = "https://api.x.com/2/tweets";
    let auth_header = build_oauth1_header(
        "POST",
        url,
        consumer_key,
        consumer_secret,
        access_token,
        access_token_secret,
    )?;
    let body = serde_json::json!({ "text": message });

    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(url)
        .header("Authorization", auth_header)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("twitter: API request failed ({status}): {body}");
    }
    Ok(())
}

fn build_oauth1_header(
    method: &str,
    url: &str,
    consumer_key: &str,
    consumer_secret: &str,
    token: &str,
    token_secret: &str,
) -> Result<String> {
    let method = method.to_uppercase();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .to_string();
    let nonce = generate_nonce();

    let mut params = BTreeMap::new();
    params.insert("oauth_consumer_key", consumer_key);
    params.insert("oauth_nonce", &nonce);
    params.insert("oauth_signature_method", "HMAC-SHA1");
    params.insert("oauth_timestamp", &timestamp);
    params.insert("oauth_token", token);
    params.insert("oauth_version", "1.0");

    let param_string: String = params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let base_string = format!(
        "{}&{}&{}",
        &method,
        percent_encode(url),
        percent_encode(&param_string)
    );
    let signing_key = format!(
        "{}&{}",
        percent_encode(consumer_secret),
        percent_encode(token_secret)
    );

    let signature = hmac_sha1_base64(signing_key.as_bytes(), base_string.as_bytes())?;

    Ok(format!(
        "OAuth oauth_consumer_key=\"{}\", oauth_nonce=\"{}\", oauth_signature=\"{}\", oauth_signature_method=\"HMAC-SHA1\", oauth_timestamp=\"{}\", oauth_token=\"{}\", oauth_version=\"1.0\"",
        percent_encode(consumer_key),
        percent_encode(&nonce),
        percent_encode(&signature),
        percent_encode(&timestamp),
        percent_encode(token),
    ))
}

fn hmac_sha1_base64(key: &[u8], data: &[u8]) -> Result<String> {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    type HmacSha1 = Hmac<Sha1>;
    let mut mac = HmacSha1::new_from_slice(key).map_err(|e| anyhow::anyhow!("HMAC error: {e}"))?;
    mac.update(data);
    Ok(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

fn percent_encode(s: &str) -> String {
    let mut result = String::new();
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                result.push(byte as char);
            }
            _ => result.push_str(&format!("%{:02X}", byte)),
        }
    }
    result
}

fn generate_nonce() -> String {
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    SystemTime::now().hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    count.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    format!("{:016x}{:016x}", hasher.finish(), count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percent_encode_basic() {
        assert_eq!(percent_encode("hello"), "hello");
        assert_eq!(percent_encode("hello world"), "hello%20world");
        assert_eq!(percent_encode("a=b&c=d"), "a%3Db%26c%3Dd");
    }

    #[test]
    fn test_percent_encode_unreserved_chars() {
        assert_eq!(percent_encode("A-Z_a~z.0"), "A-Z_a~z.0");
    }

    #[test]
    fn test_percent_encode_special_chars() {
        assert_eq!(percent_encode("/"), "%2F");
        assert_eq!(percent_encode(":"), "%3A");
        assert_eq!(percent_encode("+"), "%2B");
    }

    #[test]
    fn test_oauth1_header_format() {
        let header = build_oauth1_header(
            "POST",
            "https://api.x.com/2/tweets",
            "ck",
            "cs",
            "at",
            "ats",
        )
        .unwrap();
        assert!(header.starts_with("OAuth "));
        assert!(header.contains("oauth_consumer_key=\"ck\""));
        assert!(header.contains("oauth_token=\"at\""));
        assert!(header.contains("oauth_signature_method=\"HMAC-SHA1\""));
        assert!(header.contains("oauth_version=\"1.0\""));
        assert!(header.contains("oauth_signature="));
        assert!(header.contains("oauth_nonce="));
        assert!(header.contains("oauth_timestamp="));
    }

    #[test]
    fn test_hmac_sha1_base64_known_value() {
        // Known HMAC-SHA1 test vector
        let result =
            hmac_sha1_base64(b"key", b"The quick brown fox jumps over the lazy dog").unwrap();
        assert_eq!(result, "3nybhbi3iqa8ino29wqQcBydtNk=");
    }

    #[test]
    fn test_percent_encode_utf8() {
        // Multi-byte UTF-8 characters should be encoded byte-by-byte
        let encoded = percent_encode("caf\u{00e9}");
        assert_eq!(encoded, "caf%C3%A9");
    }

    #[test]
    fn test_generate_nonce_uniqueness() {
        let n1 = generate_nonce();
        let n2 = generate_nonce();
        assert_ne!(n1, n2);
        assert_eq!(n1.len(), 32); // 32 hex characters
    }

    #[test]
    fn test_generate_nonce_is_hex() {
        let nonce = generate_nonce();
        assert_eq!(nonce.len(), 32);
        assert!(nonce.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
