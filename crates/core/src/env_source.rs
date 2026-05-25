//! Process-env-or-injected-map abstraction for reads in production code.
//!
//! Production code reads environment variables through
//! [`Context::env_var`](crate::context::Context::env_var), which routes
//! through an injected [`EnvSource`] trait object. Production code uses
//! [`ProcessEnvSource`] (calls `std::env::var`); tests inject a
//! [`MapEnvSource`] via
//! [`TestContextBuilder::env`](crate::test_helpers::TestContextBuilder::env)
//! to drive deterministic branches without mutating the process env.
//!
//! ```no_run
//! use anodizer_core::{EnvSource, MapEnvSource, ProcessEnvSource};
//!
//! let prod = ProcessEnvSource;
//! let _ = prod.var("PATH");
//!
//! let test_src = MapEnvSource::new()
//!     .with("GITHUB_TOKEN", "ghp_synthetic")
//!     .with("CI", "true");
//! assert_eq!(test_src.var("GITHUB_TOKEN"), Some("ghp_synthetic".to_string()));
//! assert_eq!(test_src.var("MISSING"), None);
//! ```

use std::collections::HashMap;

/// Read-only lookup for an environment variable name.
///
/// Production code wires up [`ProcessEnvSource`] (which calls
/// `std::env::var`). Tests wire up [`MapEnvSource`] to drive
/// deterministic branches without mutating the process env.
pub trait EnvSource: Send + Sync {
    /// Look up `name` and return its value, or `None` if unset.
    fn var(&self, name: &str) -> Option<String>;
}

/// Production implementation that reads `std::env::var`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessEnvSource;

impl EnvSource for ProcessEnvSource {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// Map-backed implementation for tests. Built from any
/// `IntoIterator<Item=(K,V)>` (including `HashMap<K, V>` via the
/// [`From`] impl below) or fluently via [`MapEnvSource::with`].
#[derive(Debug, Clone, Default)]
pub struct MapEnvSource {
    inner: HashMap<String, String>,
}

impl MapEnvSource {
    /// Create an empty source. Use [`MapEnvSource::with`] to seed entries
    /// fluently, or [`MapEnvSource::set`] for the mutable form.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `(k, v)` and return `self` so calls can chain.
    pub fn with<K: Into<String>, V: Into<String>>(mut self, k: K, v: V) -> Self {
        self.inner.insert(k.into(), v.into());
        self
    }

    /// Insert `(k, v)` in place. Returns `&mut self` for chained mutation.
    pub fn set<K: Into<String>, V: Into<String>>(&mut self, k: K, v: V) -> &mut Self {
        self.inner.insert(k.into(), v.into());
        self
    }
}

impl<K, V> From<HashMap<K, V>> for MapEnvSource
where
    K: Into<String>,
    V: Into<String>,
{
    fn from(map: HashMap<K, V>) -> Self {
        Self {
            inner: map.into_iter().map(|(k, v)| (k.into(), v.into())).collect(),
        }
    }
}

impl EnvSource for MapEnvSource {
    fn var(&self, name: &str) -> Option<String> {
        self.inner.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::env::env_mutex;

    /// Picked deliberately weird so a real CI / dev shell will not have it
    /// set. Used by the "unset variable" tests below.
    const UNSET_VAR: &str = "ANODIZER_T3_FIXTURE_UNSET_VAR";

    #[test]
    fn process_env_source_reads_actual_env() {
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let key = "ANODIZER_T3_PROCESS_ENV_FIXTURE";
        // SAFETY: serialised by env_mutex; cleaned up before guard drop.
        unsafe { std::env::set_var(key, "from-process-env") };
        let got = ProcessEnvSource.var(key);
        // SAFETY: serialised by env_mutex.
        unsafe { std::env::remove_var(key) };
        assert_eq!(got, Some("from-process-env".to_string()));
    }

    #[test]
    fn process_env_source_returns_none_for_unset_var() {
        assert_eq!(ProcessEnvSource.var(UNSET_VAR), None);
    }

    #[test]
    fn map_env_source_returns_inserted_value() {
        let src = MapEnvSource::new().with("K", "V");
        assert_eq!(src.var("K"), Some("V".to_string()));
    }

    #[test]
    fn map_env_source_returns_none_for_missing_key() {
        let src = MapEnvSource::new().with("K", "V");
        assert_eq!(src.var("OTHER"), None);
    }

    #[test]
    fn map_env_source_from_hashmap_preserves_entries() {
        let mut m: HashMap<&str, &str> = HashMap::new();
        m.insert("A", "1");
        m.insert("B", "2");
        let src: MapEnvSource = m.into();
        assert_eq!(src.var("A"), Some("1".to_string()));
        assert_eq!(src.var("B"), Some("2".to_string()));
        assert_eq!(src.var("C"), None);
    }
}
