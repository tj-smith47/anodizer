use crate::schemastore::catalog::{Verdict, verdict};
use crate::schemastore::manifest::{
    DescriptionError, Dialect, check_id, classify_dialect, sanitize_description, slugify,
};

const CATALOG: &str = r#"{ "schemas": [
  { "name": "Aaa", "description": "a", "fileMatch": ["a"], "url": "https://x/a.json" },
  { "name": "Anodizer", "description": "d", "fileMatch": [".anodizer.yaml"], "url": "https://tj-smith47.github.io/anodizer/schema.json" }
] }"#;

#[test]
fn verdict_noop_when_entry_present_and_equal() {
    let want = serde_json::json!({
        "name": "Anodizer", "description": "d",
        "fileMatch": [".anodizer.yaml"],
        "url": "https://tj-smith47.github.io/anodizer/schema.json"
    });
    assert_eq!(verdict(CATALOG, "Anodizer", &want).unwrap(), Verdict::NoOp);
}

#[test]
fn verdict_update_when_present_but_differs() {
    let want = serde_json::json!({ "name": "Anodizer", "description": "CHANGED", "fileMatch": [".anodizer.yaml"], "url": "https://tj-smith47.github.io/anodizer/schema.json" });
    assert_eq!(
        verdict(CATALOG, "Anodizer", &want).unwrap(),
        Verdict::Update
    );
}

#[test]
fn verdict_add_when_absent() {
    let want = serde_json::json!({ "name": "Zzz", "description": "z", "fileMatch": ["z"], "url": "https://x/z.json" });
    assert_eq!(verdict(CATALOG, "Zzz", &want).unwrap(), Verdict::Add);
}

#[test]
fn slugify_lowercases_and_hyphenates() {
    assert_eq!(slugify("Anodizer"), "anodizer");
    assert_eq!(slugify("My Tool Config"), "my-tool-config");
    assert_eq!(slugify("cfgd-config"), "cfgd-config");
}

#[test]
fn description_rejects_schema_word_newline_and_trailing_punct() {
    assert!(matches!(
        sanitize_description("cfgd configuration schema"),
        Err(DescriptionError::ContainsSchemaWord)
    ));
    assert!(matches!(
        sanitize_description("line one\nline two"),
        Err(DescriptionError::ContainsNewline)
    ));
    assert!(matches!(
        sanitize_description("trailing comma,"),
        Err(DescriptionError::BadEdge)
    ));
    assert!(matches!(
        sanitize_description("   "),
        Err(DescriptionError::Empty)
    ));
    assert_eq!(
        sanitize_description("cfgd machine configuration").unwrap(),
        "cfgd machine configuration"
    );
}

#[test]
fn dialect_draft07_ok_2020_12_too_high() {
    assert_eq!(
        classify_dialect("http://json-schema.org/draft-07/schema#"),
        Dialect::Ok
    );
    assert_eq!(
        classify_dialect("https://json-schema.org/draft-07/schema#"),
        Dialect::Ok
    );
    assert_eq!(
        classify_dialect("https://json-schema.org/draft/2020-12/schema"),
        Dialect::TooHigh
    );
    assert_eq!(
        classify_dialect("https://json-schema.org/draft/2019-09/schema"),
        Dialect::TooHigh
    );
    assert_eq!(classify_dialect("ftp://nonsense"), Dialect::Unknown);
}

#[test]
fn id_must_be_http() {
    assert!(check_id(Some("https://cfgd.io/schemas/cfgd-config.schema.json")).is_ok());
    assert!(check_id(Some("urn:bad")).is_err());
    assert!(check_id(None).is_err());
}
