use anodizer_core::context::{ReleaseToken, release_skip_vocabulary};
use anyhow::Result;

/// Stable JSON envelope for `anodizer vocabulary --json`.
///
/// `schema_version` is bumped only on a breaking shape change so a pinned
/// consumer (the GitHub Action) can detect an incompatible upgrade. The token
/// list itself grows additively as publishers / stages are added.
#[derive(serde::Serialize)]
struct Vocabulary {
    schema_version: u32,
    tokens: Vec<ReleaseToken>,
}

/// Current `vocabulary` JSON schema version.
const SCHEMA_VERSION: u32 = 1;

pub struct VocabularyOpts {
    pub json: bool,
}

pub fn run(opts: VocabularyOpts) -> Result<()> {
    let vocab = Vocabulary {
        schema_version: SCHEMA_VERSION,
        tokens: release_skip_vocabulary(),
    };

    if opts.json {
        println!("{}", serde_json::to_string(&vocab)?);
    } else {
        println!("{:<20} {:<10} PUBLISH-STAGE", "TOKEN", "PUBLISHER");
        for t in &vocab.tokens {
            println!(
                "{:<20} {:<10} {}",
                t.token,
                yes_no(t.is_publisher),
                yes_no(t.is_publish_stage),
            );
        }
    }

    Ok(())
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::context::VALID_RELEASE_SKIPS;
    use std::collections::BTreeSet;

    #[test]
    fn json_envelope_carries_every_canonical_token() {
        let vocab = Vocabulary {
            schema_version: SCHEMA_VERSION,
            tokens: release_skip_vocabulary(),
        };
        let json = serde_json::to_string(&vocab).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["schema_version"], 1);
        let emitted: BTreeSet<String> = parsed["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["token"].as_str().unwrap().to_string())
            .collect();
        let valid: BTreeSet<String> = VALID_RELEASE_SKIPS.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            emitted, valid,
            "vocabulary JSON token set must equal VALID_RELEASE_SKIPS"
        );
    }

    #[test]
    fn publisher_entries_expose_both_flags() {
        let json = serde_json::to_string(&Vocabulary {
            schema_version: SCHEMA_VERSION,
            tokens: release_skip_vocabulary(),
        })
        .unwrap();
        // `blob` is a publisher that fires from a stage; `cargo` is a
        // trait-dispatched publisher; `build` is a non-publisher stage token.
        assert!(json.contains(r#"{"token":"blob","is_publisher":true,"is_publish_stage":true}"#));
        assert!(json.contains(r#"{"token":"cargo","is_publisher":true,"is_publish_stage":false}"#));
        assert!(
            json.contains(r#"{"token":"build","is_publisher":false,"is_publish_stage":false}"#)
        );
    }

    #[test]
    fn run_json_and_human_both_succeed() {
        assert!(run(VocabularyOpts { json: true }).is_ok());
        assert!(run(VocabularyOpts { json: false }).is_ok());
    }
}
