//! Digest builtins: one function per supported algorithm, each hashing the
//! contents of the file named by its `s` argument and returning lowercase hex.

use serde_json::Value;
use std::collections::HashMap;
use tera::TeraResult;

use crate::template::engine_adapter::JsonRegisterExt;

use sha1::Digest as Sha1Digest;
use sha2::Digest as Sha2Digest;
use sha3::Digest as Sha3Digest;

pub(super) fn register(tera: &mut tera::Tera) {
    // --- Hash functions (all 14 algorithms) ---

    macro_rules! register_hash_fn {
        ($tera:expr, $name:expr, $hash_fn:expr) => {
            $tera.register_json_function(
                $name,
                |args: &HashMap<String, Value>| -> TeraResult<Value> {
                    let s = args.get("s").and_then(|v| v.as_str()).ok_or_else(|| {
                        tera::Error::message(format!("{} requires `s` argument", $name))
                    })?;
                    // Read the file; error if it cannot be read (no silent fallback).
                    let bytes = std::fs::read(s).map_err(|e| {
                        tera::Error::message(format!(
                            "{}: failed to read file '{}': {}",
                            $name, s, e
                        ))
                    })?;
                    Ok(Value::String($hash_fn(&bytes)))
                },
            );
        };
    }

    register_hash_fn!(tera, "sha1", |b: &[u8]| {
        let mut h = sha1::Sha1::new();
        Sha1Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha1Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha224", |b: &[u8]| {
        let mut h = sha2::Sha224::new();
        Sha2Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha2Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha256", |b: &[u8]| {
        let mut h = sha2::Sha256::new();
        Sha2Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha2Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha384", |b: &[u8]| {
        let mut h = sha2::Sha384::new();
        Sha2Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha2Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha512", |b: &[u8]| {
        let mut h = sha2::Sha512::new();
        Sha2Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha2Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha3_224", |b: &[u8]| {
        let mut h = sha3::Sha3_224::new();
        Sha3Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha3Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha3_256", |b: &[u8]| {
        let mut h = sha3::Sha3_256::new();
        Sha3Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha3Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha3_384", |b: &[u8]| {
        let mut h = sha3::Sha3_384::new();
        Sha3Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha3Digest::finalize(h))
    });
    register_hash_fn!(tera, "sha3_512", |b: &[u8]| {
        let mut h = sha3::Sha3_512::new();
        Sha3Digest::update(&mut h, b);
        crate::hashing::hex_lower(&Sha3Digest::finalize(h))
    });
    register_hash_fn!(tera, "blake2b", |b: &[u8]| {
        let mut h = blake2::Blake2b512::new();
        blake2::Digest::update(&mut h, b);
        crate::hashing::hex_lower(&blake2::Digest::finalize(h))
    });
    register_hash_fn!(tera, "blake2s", |b: &[u8]| {
        let mut h = blake2::Blake2s256::new();
        blake2::Digest::update(&mut h, b);
        crate::hashing::hex_lower(&blake2::Digest::finalize(h))
    });
    register_hash_fn!(tera, "blake3", |b: &[u8]| {
        crate::hashing::hex_lower(blake3::hash(b).as_bytes())
    });
    register_hash_fn!(tera, "md5", |b: &[u8]| {
        let mut h = md5::Md5::new();
        md5::Digest::update(&mut h, b);
        crate::hashing::hex_lower(&md5::Digest::finalize(h))
    });
    register_hash_fn!(tera, "crc32", |b: &[u8]| {
        format!("{:08x}", crc32fast::hash(b))
    });
}
