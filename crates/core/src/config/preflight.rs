//! Pre-publish preflight configuration.
//!
//! The `preflight:` block tunes the live publisher-state / credential probes
//! that run before any publisher mutates an external registry. The probes
//! themselves are always read-only; this block only changes how their
//! outcomes gate the release.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Top-level `preflight:` block.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PreflightConfig {
    /// Promote INDETERMINATE preflight outcomes to hard blockers. An
    /// indeterminate outcome is one where a probe could not reach a verdict —
    /// a 5xx, a 429 / rate-limit, a transport failure, or a response that
    /// hides the permission the publish path needs. By default those degrade
    /// to warnings so a transient upstream blip cannot abort a release whose
    /// credentials are actually valid; `strict: true` makes them abort
    /// instead (fail-closed). Definitive failures (credentials rejected,
    /// target missing) keep their required→blocker / optional→warning
    /// severity regardless of this setting. Equivalent to passing
    /// `--strict-preflight` (or the global `--strict`) on every run.
    pub strict: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_yaml_yields_defaults() {
        let c: PreflightConfig = serde_yaml_ng::from_str("{}").unwrap();
        assert_eq!(c, PreflightConfig::default());
        assert!(!c.strict, "strict is opt-in");
    }

    #[test]
    fn strict_true_parses() {
        let c: PreflightConfig = serde_yaml_ng::from_str("strict: true").unwrap();
        assert!(c.strict);
    }

    #[test]
    fn unknown_field_rejected() {
        let res: Result<PreflightConfig, _> = serde_yaml_ng::from_str("strict: true\nbogus: 1\n");
        assert!(res.is_err(), "deny_unknown_fields must reject typos");
    }
}
