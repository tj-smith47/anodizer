//! Hex → Nix-native base32 / SRI hash conversions.
//!
//! Hashes are converted using `nix-hash --type sha256 --flat --base32`.
//! Nix uses a non-standard base32 encoding with the alphabet
//! `0123456789abcdfghijklmnpqrsvwxyz` (32 chars, omitting e/o/t/u).

use anyhow::Result;

/// Convert a hex-encoded SHA256 hash to nix-native base32 format.
///
/// Hashes are converted using `nix-hash --type sha256 --flat --base32`.
/// Nix uses a non-standard base32 encoding with the alphabet
/// `0123456789abcdfghijklmnpqrsvwxyz` (32 chars, omitting e/o/t/u).
/// Bytes are processed in reverse order.
pub fn hex_sha256_to_nix_base32(hex: &str) -> Result<String> {
    let bytes = hex_to_bytes(hex)?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "nix: expected 32 bytes for SHA256 hash, got {} (hex: '{}')",
            bytes.len(),
            hex
        );
    }
    Ok(nix_base32_encode(&bytes))
}

/// Nix base32 alphabet (omits e, o, t, u from standard base32).
const NIX_BASE32_CHARS: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// Encode raw bytes into nix-native base32 format.
///
/// This is a faithful port of Nix's `printHash32()` from
/// `src/libutil/hash.cc`.  The algorithm reads bits from the raw hash
/// bytes starting at the highest bit positions and emits characters
/// from left to right.
fn nix_base32_encode(bytes: &[u8]) -> String {
    let hash_size = bytes.len();
    let len = hash_size * 8 / 5 + usize::from(!(hash_size * 8).is_multiple_of(5));
    let mut out = vec![b'0'; len];

    let mut n = len;
    while n > 0 {
        n -= 1;
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        let c = if i >= hash_size - 1 {
            (bytes[i] as u16) >> j
        } else {
            ((bytes[i] as u16) >> j) | ((bytes[i + 1] as u16) << (8 - j))
        } as u8;
        out[len - 1 - n] = NIX_BASE32_CHARS[(c & 0x1f) as usize];
    }

    // `out` contains only ASCII characters from NIX_BASE32_CHARS, so
    // from_utf8 cannot fail; fall back to an empty string in the
    // impossible error case rather than panic.
    String::from_utf8(out).unwrap_or_default()
}

/// Convert a hex-encoded SHA256 hash to SRI format (`sha256-{base64}`).
///
/// Alternative to nix-native base32; SRI is accepted by Nix 2.4+.
#[cfg(test)]
pub fn hex_sha256_to_sri(hex: &str) -> Result<String> {
    use base64::Engine as _;
    let bytes = hex_to_bytes(hex)?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "nix: expected 32 bytes for SHA256 hash, got {} (hex: '{}')",
            bytes.len(),
            hex
        );
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("sha256-{}", b64))
}

/// Decode a hex string into raw bytes.
fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        anyhow::bail!("nix: hex string has odd length: '{}'", hex);
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| anyhow::anyhow!("nix: invalid hex at offset {}: {}", i, e))
        })
        .collect()
}
