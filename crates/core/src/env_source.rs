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
use std::sync::Arc;

/// Read-only lookup for an environment variable name.
///
/// Production code wires up [`ProcessEnvSource`] (which calls
/// `std::env::var`). Tests wire up [`MapEnvSource`] to drive
/// deterministic branches without mutating the process env.
pub trait EnvSource: Send + Sync {
    /// Look up `name` and return its value, or `None` if unset.
    fn var(&self, name: &str) -> Option<String>;

    /// Snapshot every `(name, value)` pair this source can enumerate.
    ///
    /// Callers that need to scan the whole env (e.g. the determinism
    /// harness's Windows inherit-everything pass, which drops a
    /// credential deny-list out of the host env) use this instead of
    /// `std::env::vars()` so a test can inject a closed map of fixture
    /// entries.
    ///
    /// Required (no default): log/announce secret redaction builds its
    /// mask table from this snapshot, so a source that returned an empty
    /// `Vec` here would silently disable redaction while `var(...)` lookups
    /// kept working — a silent-failure footgun. Forcing every impl to
    /// provide `vars()` turns that mistake into a compile error. A source
    /// that genuinely cannot enumerate must return its full point-lookup
    /// domain (or, if truly unbounded, be reworked — never fall back to an
    /// empty snapshot).
    fn vars(&self) -> Vec<(String, String)>;
}

/// Production implementation that reads `std::env::var`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessEnvSource;

impl EnvSource for ProcessEnvSource {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn vars(&self) -> Vec<(String, String)> {
        std::env::vars().collect()
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

    fn vars(&self) -> Vec<(String, String)> {
        self.inner
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// An [`EnvSource`] that overlays a small map of override entries on top of a
/// base source.
///
/// A lookup returns the override value when the name is present **and
/// non-empty**; otherwise it delegates to the base source. This lets a caller
/// inject one or two synthetic variables (e.g. a short-lived credential minted
/// at runtime) so that env-driven code paths — scope-availability probes,
/// token resolvers — observe the injected value without mutating the process
/// environment or discarding the base source's other variables.
///
/// An empty override string is treated as "not set" and falls through to the
/// base, matching how the rest of the codebase treats an empty credential
/// (`is_some_and(|t| !t.is_empty())`): overlaying `("X", "")` cannot mask a
/// real base value.
///
/// ```
/// use std::sync::Arc;
/// use anodizer_core::{EnvSource, LayeredEnvSource, MapEnvSource};
///
/// let base: Arc<dyn EnvSource> = Arc::new(MapEnvSource::new().with("A", "base-a"));
/// let layered = LayeredEnvSource::new(base, [("A", "override-a"), ("B", "override-b")]);
/// assert_eq!(layered.var("A"), Some("override-a".to_string())); // override wins
/// assert_eq!(layered.var("B"), Some("override-b".to_string())); // override-only key
/// assert_eq!(layered.var("MISSING"), None);                     // neither has it
/// ```
#[derive(Clone)]
pub struct LayeredEnvSource {
    base: Arc<dyn EnvSource>,
    overrides: HashMap<String, String>,
}

impl LayeredEnvSource {
    /// Wrap `base` with the given `(name, value)` overrides. Empty override
    /// values are retained but treated as absent by [`LayeredEnvSource::var`]
    /// (they fall through to the base).
    pub fn new<I, K, V>(base: Arc<dyn EnvSource>, overrides: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            base,
            overrides: overrides
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }
}

impl EnvSource for LayeredEnvSource {
    fn var(&self, name: &str) -> Option<String> {
        match self.overrides.get(name) {
            Some(v) if !v.is_empty() => Some(v.clone()),
            _ => self.base.var(name),
        }
    }

    fn vars(&self) -> Vec<(String, String)> {
        // Start from the base snapshot, then apply non-empty overrides so a
        // whole-env scan (e.g. the determinism inherit-everything pass) sees
        // the overlaid value in place of the base one.
        let mut merged: HashMap<String, String> = self.base.vars().into_iter().collect();
        for (k, v) in &self.overrides {
            if !v.is_empty() {
                merged.insert(k.clone(), v.clone());
            }
        }
        merged.into_iter().collect()
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
        // env-ok: this is the contract test for ProcessEnvSource — it must
        // observe the *real* process env, so there is no injection seam to
        // route through; env_mutex serialises the unique-key mutation.
        // SAFETY: serialised by env_mutex; cleaned up before guard drop.
        // env-ok: ProcessEnvSource contract test; env_mutex-guarded, unique key
        unsafe { std::env::set_var(key, "from-process-env") };
        let got = ProcessEnvSource.var(key);
        // SAFETY: serialised by env_mutex.
        // env-ok: ProcessEnvSource contract test; env_mutex-guarded, unique key
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
    fn layered_env_source_override_wins_over_base() {
        let base: Arc<dyn EnvSource> = Arc::new(MapEnvSource::new().with("K", "base"));
        let layered = LayeredEnvSource::new(base, [("K", "overridden")]);
        assert_eq!(layered.var("K"), Some("overridden".to_string()));
    }

    #[test]
    fn layered_env_source_empty_override_falls_through_to_base() {
        let base: Arc<dyn EnvSource> = Arc::new(MapEnvSource::new().with("K", "base"));
        let layered = LayeredEnvSource::new(base, [("K", "")]);
        // An empty override must not mask the real base value.
        assert_eq!(layered.var("K"), Some("base".to_string()));
    }

    #[test]
    fn layered_env_source_unset_falls_through_to_base() {
        let base: Arc<dyn EnvSource> = Arc::new(MapEnvSource::new().with("BASE_ONLY", "b"));
        let layered = LayeredEnvSource::new(base, [("OTHER", "o")]);
        // A name only the base knows resolves through the base.
        assert_eq!(layered.var("BASE_ONLY"), Some("b".to_string()));
        // A name neither knows is None.
        assert_eq!(layered.var("NEITHER"), None);
        // The override-only name still resolves.
        assert_eq!(layered.var("OTHER"), Some("o".to_string()));
    }

    #[test]
    fn layered_env_source_vars_merges_base_and_overrides() {
        let base: Arc<dyn EnvSource> =
            Arc::new(MapEnvSource::new().with("A", "base-a").with("B", "b"));
        let layered = LayeredEnvSource::new(base, [("A", "over-a"), ("C", "c")]);
        let map: HashMap<String, String> = layered.vars().into_iter().collect();
        assert_eq!(map.get("A").map(String::as_str), Some("over-a"));
        assert_eq!(map.get("B").map(String::as_str), Some("b"));
        assert_eq!(map.get("C").map(String::as_str), Some("c"));
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
