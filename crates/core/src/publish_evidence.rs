use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishEvidence {
    pub schema_version: u32,
    pub publisher: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_ref: Option<String>,
    pub artifact_paths: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nondeterministic: Option<String>,
    /// Free-form operator-public metadata for the publisher run.
    ///
    /// **CREDENTIAL CONTRACT**: this field is persisted to
    /// `dist/run-<id>/report.json`, summarised in `summary.json`, and
    /// may be attached to the GitHub Release body via the announce
    /// stage. It MUST contain only operator-public identifiers (URLs,
    /// env-var NAMES, PR numbers, tag strings, branch names). Token
    /// VALUES, private keys, passwords, OAuth secrets, SSH key
    /// material, or any other credential bytes MUST be
    /// `#[serde(skip)]`d on any type whose serialized form lands here
    /// — and resolved at rollback time from the live process env
    /// (see e.g. `HomebrewTarget::token_env_var`).
    ///
    /// Each publisher carries a `<name>_target_extra_carries_no_secret_material`
    /// regression test that grep-asserts the rendered JSON contains
    /// no `"token":` / `"password":` / `"pat":` / `"private_key":`
    /// field. New publishers MUST add an equivalent test alongside
    /// their `*Target` struct.
    pub extra: serde_json::Value,
}

impl PublishEvidence {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    pub fn new(publisher: impl Into<String>) -> Self {
        Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            publisher: publisher.into(),
            primary_ref: None,
            artifact_paths: Vec::new(),
            nondeterministic: None,
            extra: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_evidence_roundtrips_through_json() {
        let mut e = PublishEvidence::new("homebrew");
        e.primary_ref = Some("refs/heads/main".to_string());
        e.artifact_paths.push(PathBuf::from("dist/foo.tar.gz"));
        e.nondeterministic = Some("timestamp".to_string());
        e.extra = serde_json::json!({"pr_url": "https://example.com/pr/1"});

        let s = serde_json::to_string(&e).expect("serialize");
        let back: PublishEvidence = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(e, back);
    }

    #[test]
    fn publish_evidence_omits_none_fields_on_serialize() {
        // `primary_ref` and `nondeterministic` default to None on a
        // fresh evidence; with `skip_serializing_if = Option::is_none`
        // they must not appear in the rendered JSON. Round-trip stays
        // clean because serde decodes a missing `Option<T>` field as
        // `None` by default.
        let e = PublishEvidence::new("homebrew");
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(
            !s.contains("primary_ref"),
            "primary_ref should be omitted when None: {s}"
        );
        assert!(
            !s.contains("nondeterministic"),
            "nondeterministic should be omitted when None: {s}"
        );
        let back: PublishEvidence = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(e, back);
    }

    #[test]
    fn publish_evidence_rejects_unknown_fields() {
        let bad = r#"{
            "schema_version": 1,
            "publisher": "homebrew",
            "primary_ref": null,
            "artifact_paths": [],
            "nondeterministic": null,
            "extra": null,
            "future_field": "boom"
        }"#;
        let r: Result<PublishEvidence, _> = serde_json::from_str(bad);
        assert!(r.is_err(), "deny_unknown_fields should reject future_field");
    }
}
