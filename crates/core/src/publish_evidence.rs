use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishEvidence {
    pub schema_version: u32,
    pub publisher: String,
    pub primary_ref: Option<String>,
    pub artifact_paths: Vec<PathBuf>,
    pub nondeterministic: Option<String>,
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
