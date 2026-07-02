//! Process-wide rustls [`CryptoProvider`] selection.
//!
//! anodizer builds every TLS path it owns directly on `ring` (a pure-Rust,
//! hermetically cross-compilable provider — no C/cmake/asm toolchain, which
//! the determinism harness's musl / windows-aarch64 / zigbuild rebuilds
//! depend on). Under rustls 0.23, though, `aws-lc-rs` is the *default*
//! provider, and transitive TLS users that take the default — `lettre`'s
//! `rustls-tls`, `reqwest`'s `rustls-tls`, `object_store` — compile it into
//! the same binary. With two providers linked, rustls refuses to guess and
//! [panics] the first time a `ClientConfig`/`ServerConfig` builder is created
//! without an explicit provider ("Could not automatically determine the
//! process-level CryptoProvider").
//!
//! Installing `ring` as the process default at startup makes that selection
//! deterministic, so no present or future bare-builder path can panic on
//! provider ambiguity — the incoherence is removed by construction rather
//! than left to every call site to remember. Paths that already pass an
//! explicit provider (the GitHub API client) keep using `ring`. `lettre`
//! owns its provider internally (`rustls::ClientConfig::builder_with_provider`
//! in its `smtp` transport) and is genuinely unaffected. `reqwest` and
//! `object_store`, however, link both `ring` and `aws-lc-rs` and take
//! rustls's *process-default* `CryptoProvider` for their bare
//! `ClientConfig`/`ServerConfig` builders — they rely on this module's pin
//! rather than owning their own provider.
//!
//! [panics]: https://docs.rs/rustls/0.23/rustls/crypto/struct.CryptoProvider.html#method.install_default

/// Pin the process-default rustls [`CryptoProvider`] to `ring`.
///
/// Call once, as early as possible, before any TLS handshake. Idempotent and
/// infallible to the caller: a second call (or a prior install, e.g. from a
/// test harness) leaves the existing default in place rather than erroring.
///
/// [`CryptoProvider`]: rustls::crypto::CryptoProvider
pub fn install_default_crypto_provider() {
    // `install_default` returns `Err` if a provider is already installed; we
    // standardise on `ring` and treat an existing install as already-correct.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
