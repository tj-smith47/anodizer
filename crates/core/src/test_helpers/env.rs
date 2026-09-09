//! The two halves of a race-free env mutation in a test: [`env_mutex`]
//! serialises it, [`EnvGuard`] undoes it.
//!
//! `cargo test` parallelises tests within a single binary, so any test that
//! mutates the process environment (`std::env::set_var` / `remove_var`) races
//! against other tests reading those variables. Hold the mutex across the
//! mutation to serialise without standing up a per-file mutex in every test
//! module; a `#[serial_test::serial(<group>)]` covering every test that
//! touches the same variable serves the same purpose.
//!
//! Never mutate the env by hand and restore it by hand: the restore is skipped
//! whenever the body between the two halves panics, and the override leaks
//! into whatever test runs next in the process. `EnvGuard` restores on drop,
//! unwinding included, so the paired-call shape has no reason to exist.
//!
//! ```no_run
//! use anodizer_core::test_helpers::env::{EnvGuard, env_mutex};
//!
//! let _lock = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
//! let _foo = EnvGuard::set("FOO", "1");
//! let _bar = EnvGuard::remove("BAR");
//! // ... test body; both restored when the guards drop ...
//! ```
//!
//! Recovering from a poisoned lock (`.unwrap_or_else(|e| e.into_inner())`)
//! is intentional: a panicking test that holds the guard pollutes the
//! mutex state, but subsequent tests still want to serialise correctly.

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
/// Callers must serialise the mutation for the guard's lifetime, by holding
/// [`env_mutex`] or by carrying a `#[serial_test::serial(<group>)]` covering
/// every test that touches the same variable, so the process-global env is
/// mutated by one test at a time.
///
/// ```no_run
/// use anodizer_core::test_helpers::env::{EnvGuard, env_mutex};
///
/// let _lock = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
/// let _path = EnvGuard::set("PATH", "/stub/bin");
/// let _token = EnvGuard::remove("GITHUB_TOKEN");
/// // both restored when the guards drop, panic or not
/// ```
pub struct EnvGuard(&'static str, Option<std::ffi::OsString>);

impl EnvGuard {
    /// Set `key=val`, remembering the prior value for restoration on drop.
    pub fn set(key: &'static str, val: impl AsRef<std::ffi::OsStr>) -> Self {
        // `var_os`, not `var`: a prior value this process did not set may not
        // be UTF-8, and restoring it as absent would be a leak of its own.
        let prev = std::env::var_os(key);
        // SAFETY: serialized by the caller; restored on drop.
        unsafe { std::env::set_var(key, val) };
        Self(key, prev)
    }

    /// Remove `key` for the guard's lifetime, remembering the prior value for
    /// restoration on drop. The counterpart to [`set`](Self::set) for a test
    /// that must prove behaviour with the variable ABSENT — an ambient value
    /// on the host would otherwise mask exactly the path under test.
    pub fn remove(key: &'static str) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: serialized by the caller; restored on drop.
        unsafe { std::env::remove_var(key) };
        Self(key, prev)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: serialized by the caller for the guard's life.
        unsafe {
            match &self.1 {
                Some(v) => std::env::set_var(self.0, v),
                None => std::env::remove_var(self.0),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EnvGuard, env_mutex};

    /// The guard's whole reason to exist: the paired set/restore idiom it
    /// replaces restores nothing when the body between the two halves unwinds,
    /// leaking the override into whatever test runs next in the process.
    #[test]
    fn a_panic_under_the_guard_still_restores_the_prior_value() {
        let _lock = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let before = std::env::var_os("PATH");

        let unwound = std::panic::catch_unwind(|| {
            let _guard = EnvGuard::set("PATH", "/anodizer-stub-path-only");
            assert_eq!(
                std::env::var("PATH").as_deref(),
                Ok("/anodizer-stub-path-only")
            );
            panic!("the body unwinds while the stub PATH is live");
        });

        assert!(unwound.is_err(), "the closure must have panicked");
        assert_eq!(
            std::env::var_os("PATH"),
            before,
            "PATH must be restored on unwind"
        );
    }

    /// `remove` is `set`'s mirror: the variable is absent for the guard's
    /// life and its prior value — not its absence — comes back on drop.
    #[test]
    fn remove_restores_the_prior_value_on_drop() {
        let _lock = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let key = "ANODIZER_ENV_GUARD_REMOVE_FIXTURE";
        let _outer = EnvGuard::set(key, "outer");

        {
            let _cleared = EnvGuard::remove(key);
            assert_eq!(std::env::var_os(key), None, "the guard must clear it");
        }

        assert_eq!(
            std::env::var(key).as_deref(),
            Ok("outer"),
            "dropping the remove guard must restore the prior value"
        );
    }

    /// A variable that was absent before the guard stays absent after it.
    #[test]
    fn remove_of_an_unset_variable_leaves_it_unset() {
        let _lock = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let key = "ANODIZER_ENV_GUARD_REMOVE_UNSET_FIXTURE";
        drop(EnvGuard::remove(key));
        assert_eq!(std::env::var_os(key), None);
    }
}
