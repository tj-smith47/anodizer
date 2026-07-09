//! Shared global mutex for env-mutating tests.
//!
//! `cargo test` parallelises tests within a single binary, so any test
//! that mutates the process environment (`std::env::set_var` /
//! `remove_var`) races against other tests reading those variables.
//! Hold this mutex across the env-mutation block to serialise without
//! standing up a per-file mutex in every test module.
//!
//! Usage:
//!
//! ```no_run
//! use anodizer_core::test_helpers::env::env_mutex;
//!
//! let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
//! // SAFETY: serialised by the mutex above; pair set / remove.
//! unsafe { std::env::set_var("FOO", "1") };
//! // ... test body ...
//! unsafe { std::env::remove_var("FOO") };
//! ```
//!
//! Recovering from a poisoned lock (`.unwrap_or_else(|e| e.into_inner())`)
//! is intentional: a panicking test that holds the guard pollutes the
//! mutex state, but subsequent tests still want to serialise correctly.
//! Cleaning the env var on test-body unwind is the test author's
//! responsibility — wrap in a scope guard if the test panics
//! unpredictably.

use std::sync::{Mutex, OnceLock};

/// Process-wide mutex shared by every test that mutates the env. Lazily
/// initialised on first call; safe to drop the returned guard with
/// `let _g = env_mutex().lock()....`
pub fn env_mutex() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

/// RAII env-var override that restores the prior value on drop, so a panicking
/// assertion cannot leak a mutated env into a sibling test — the scope guard
/// the module doc above says is otherwise the test author's responsibility.
/// Callers must hold [`env_mutex`] for the guard's lifetime so the
/// process-global env is mutated by one test at a time.
pub struct EnvGuard(&'static str, Option<String>);

impl EnvGuard {
    /// Set `key=val`, remembering the prior value for restoration on drop.
    pub fn set(key: &'static str, val: &str) -> Self {
        let prev = std::env::var(key).ok();
        // SAFETY: serialized by env_mutex (caller-held); restored on drop.
        unsafe { std::env::set_var(key, val) };
        Self(key, prev)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: serialized by env_mutex (caller-held for the guard's life).
        unsafe {
            match &self.1 {
                Some(v) => std::env::set_var(self.0, v),
                None => std::env::remove_var(self.0),
            }
        }
    }
}
