use crate::schemastore::catalog::{Verdict, build_entry_json, splice_entry, verdict};
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

#[test]
fn splice_appends_without_touching_other_entries() {
    let catalog = "{\n  \"schemas\": [\n    {\n      \"name\": \"Aaa\",\n      \"description\": \"a\",\n      \"fileMatch\": [\"a\"],\n      \"url\": \"https://x/a.json\"\n    }\n  ]\n}\n";
    let entry = serde_json::json!({ "name": "Zzz", "description": "z", "fileMatch": ["z"], "url": "https://x/z.json" });
    let out = splice_entry(catalog, "Zzz", &entry).unwrap();
    assert!(
        out.contains("\"name\": \"Aaa\""),
        "existing entry preserved"
    );
    assert!(out.contains("\"name\": \"Zzz\""), "new entry added");
    assert!(out.contains("      \"description\": \"a\",\n      \"fileMatch\": [\"a\"],"));
    serde_json::from_str::<serde_json::Value>(&out).unwrap();
}

#[test]
fn splice_replaces_existing_entry_in_place() {
    let catalog = "{\n  \"schemas\": [\n    {\n      \"name\": \"Anodizer\",\n      \"description\": \"old\",\n      \"fileMatch\": [\".anodizer.yaml\"],\n      \"url\": \"https://u/old.json\"\n    }\n  ]\n}\n";
    let entry = serde_json::json!({ "name": "Anodizer", "description": "new", "fileMatch": [".anodizer.yaml"], "url": "https://u/new.json" });
    let out = splice_entry(catalog, "Anodizer", &entry).unwrap();
    assert!(out.contains("\"description\": \"new\""));
    assert!(!out.contains("\"description\": \"old\""));
    serde_json::from_str::<serde_json::Value>(&out).unwrap();
}

#[test]
fn build_entry_json_orders_keys_canonically() {
    let e = build_entry_json(
        "Anodizer",
        "d",
        &[".anodizer.yaml".into()],
        "https://u/s.json",
        None,
    );
    let s = serde_json::to_string(&e).unwrap();
    let (np, dp, fp, up) = (
        s.find("name").unwrap(),
        s.find("description").unwrap(),
        s.find("fileMatch").unwrap(),
        s.find("url").unwrap(),
    );
    assert!(np < dp && dp < fp && fp < up);
}

/// A naive depth counter would count the `{`/`}`/`[`/`]` literals embedded in
/// the `description` string and miscompute the entry span — this proves the
/// scanner tracks string state.
#[test]
fn find_entry_span_ignores_braces_in_strings() {
    let catalog = "{\n  \"schemas\": [\n    {\n      \"name\": \"Brace\",\n      \"description\": \"has { and } and [ ] inside\",\n      \"fileMatch\": [\"b\"],\n      \"url\": \"https://x/b.json\"\n    },\n    {\n      \"name\": \"After\",\n      \"description\": \"plain\",\n      \"fileMatch\": [\"c\"],\n      \"url\": \"https://x/c.json\"\n    }\n  ]\n}\n";
    let entry = serde_json::json!({ "name": "Brace", "description": "replaced", "fileMatch": ["b"], "url": "https://x/b.json" });
    let out = splice_entry(catalog, "Brace", &entry).unwrap();
    assert!(out.contains("\"description\": \"replaced\""));
    assert!(!out.contains("has { and } and [ ] inside"));
    // The sibling entry after the brace-laden one must be untouched.
    assert!(out.contains("\"name\": \"After\""));
    assert!(out.contains("\"description\": \"plain\""));
    serde_json::from_str::<serde_json::Value>(&out).unwrap();
}

/// An escaped quote `\"` inside a string value must not toggle string state;
/// a naive scanner would treat the `]`/`}` after it as structural and break.
#[test]
fn find_array_close_ignores_escaped_quotes() {
    let catalog = "{\n  \"schemas\": [\n    {\n      \"name\": \"Quoted\",\n      \"description\": \"a \\\"quote\\\" with } brace\",\n      \"fileMatch\": [\"q\"],\n      \"url\": \"https://x/q.json\"\n    }\n  ]\n}\n";
    let entry = serde_json::json!({ "name": "New", "description": "z", "fileMatch": ["z"], "url": "https://x/z.json" });
    let out = splice_entry(catalog, "New", &entry).unwrap();
    // Append path: the new entry lands inside the array, before the closing `]`.
    assert!(
        out.contains("\"name\": \"Quoted\""),
        "existing entry preserved"
    );
    assert!(
        out.contains("\"name\": \"New\""),
        "new entry appended inside array"
    );
    serde_json::from_str::<serde_json::Value>(&out).unwrap();
    // The appended entry must be a sibling of "Quoted", i.e. both inside the
    // same `schemas` array — proven by valid JSON with both names present
    // and the array still having exactly one closing bracket structure.
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let arr = v["schemas"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
}

#[test]
fn splice_appends_into_empty_array() {
    let catalog = "{\n  \"schemas\": []\n}\n";
    let entry = serde_json::json!({ "name": "First", "description": "f", "fileMatch": ["f"], "url": "https://x/f.json" });
    let out = splice_entry(catalog, "First", &entry).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let arr = v["schemas"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "First");
}
