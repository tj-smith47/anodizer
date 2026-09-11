use crate::schemastore::catalog::{
    Verdict, add_high_schema_version, build_entry_json, merge_versions, schema_options_block,
    splice_entry, upsert_schema_options, upstream_versions_for, verdict,
};
use crate::schemastore::manifest::{
    DescriptionError, Dialect, check_id, classify_dialect, format_vendor_schema,
    sanitize_description, slugify, unknown_formats,
};
use crate::schemastore::scan::jsonc_array_contains;

const CATALOG: &str = r#"{ "schemas": [
  { "name": "Aaa", "description": "a", "fileMatch": ["a"], "url": "https://x/a.json" },
  { "name": "Anodizer", "description": "d", "fileMatch": [".anodizer.yaml"], "url": "https://tj-smith47.github.io/anodizer/schema.json" }
] }"#;

/// The desired entry anodizer's own `.anodizer.yaml` emits, byte-equal to the
/// entry merged into SchemaStore's live catalog on 2026-05-24 (title-case
/// `Anodizer`, both `.anodizer.{yaml,yml}` globs). Against that catalog the
/// verdict MUST be `NoOp` so no further PR is ever generated. Probed live
/// against the real catalog (`name="Anodizer"` ⇒ NoOp, `name="anodizer"` ⇒
/// Update); this pins that contract with a self-contained fixture so the suite
/// stays offline.
#[test]
fn verdict_noop_for_live_anodizer_catalog_entry() {
    let fm = vec![".anodizer.yaml".to_string(), ".anodizer.yml".to_string()];
    let description = "Anodizer Rust release-automation configuration file";
    let url = "https://tj-smith47.github.io/anodizer/schema.json";
    let merged = build_entry_json("Anodizer", description, &fm, url, None);
    let catalog = serde_json::json!({ "schemas": [merged] }).to_string();

    let want = build_entry_json("Anodizer", description, &fm, url, None);
    assert_eq!(verdict(&catalog, &want).unwrap(), Verdict::NoOp);

    // A lowercase desired name would rename the entry in place (Update), never
    // append a duplicate — the bug this fix closes.
    let lower = build_entry_json("anodizer", description, &fm, url, None);
    assert_eq!(verdict(&catalog, &lower).unwrap(), Verdict::Update);
}

#[test]
fn verdict_noop_when_entry_present_and_equal() {
    let want = serde_json::json!({
        "name": "Anodizer", "description": "d",
        "fileMatch": [".anodizer.yaml"],
        "url": "https://tj-smith47.github.io/anodizer/schema.json"
    });
    assert_eq!(verdict(CATALOG, &want).unwrap(), Verdict::NoOp);
}

#[test]
fn verdict_update_when_present_but_differs() {
    let want = serde_json::json!({ "name": "Anodizer", "description": "CHANGED", "fileMatch": [".anodizer.yaml"], "url": "https://tj-smith47.github.io/anodizer/schema.json" });
    assert_eq!(verdict(CATALOG, &want).unwrap(), Verdict::Update);
}

#[test]
fn verdict_add_when_absent() {
    let want = serde_json::json!({ "name": "Zzz", "description": "z", "fileMatch": ["z"], "url": "https://x/z.json" });
    assert_eq!(verdict(CATALOG, &want).unwrap(), Verdict::Add);
}

/// The merged upstream catalog entry is title-case `Anodizer`; a desired entry
/// whose name drifts in case (`anodizer`) still MATCHES it by `fileMatch`-
/// overlap, so the verdict is `Update` (rename in place) — NOT `Add`. A
/// name-keyed verdict would have returned `Add` and appended the duplicate
/// `fileMatch` SchemaStore CI rejects; matching by `fileMatch` is what closes
/// that bug. (Full structural equality — including name — is required for
/// `NoOp`; a case drift is a real difference, hence `Update`.)
#[test]
fn verdict_update_on_filematch_despite_name_case_drift() {
    let want = serde_json::json!({
        "name": "anodizer", "description": "d",
        "fileMatch": [".anodizer.yaml"],
        "url": "https://tj-smith47.github.io/anodizer/schema.json"
    });
    assert_eq!(verdict(CATALOG, &want).unwrap(), Verdict::Update);
}

/// `fileMatch` overlaps the merged entry but other fields differ → Update, NOT
/// Add. A name-keyed verdict would have returned Add here and appended a
/// duplicate `fileMatch` SchemaStore CI rejects.
#[test]
fn verdict_update_when_filematch_overlaps_but_name_and_fields_differ() {
    let want = serde_json::json!({
        "name": "anodizer", "description": "CHANGED",
        "fileMatch": [".anodizer.yaml", ".anodizer.yml"],
        "url": "https://example.com/new.json"
    });
    assert_eq!(verdict(CATALOG, &want).unwrap(), Verdict::Update);
}

/// With neither half of the identity rule satisfied — no `fileMatch` overlap
/// (the intersection is vacuously empty) and no `name` match — the verdict is
/// `Add`, never a false `Update`/`NoOp` against an unrelated entry.
#[test]
fn verdict_add_when_neither_filematch_nor_name_matches() {
    // Desired entry carries an empty `fileMatch` array and an unrelated name.
    let empty_fm = serde_json::json!({
        "name": "Unrelated", "description": "d", "fileMatch": [],
        "url": "https://tj-smith47.github.io/anodizer/schema.json"
    });
    assert_eq!(verdict(CATALOG, &empty_fm).unwrap(), Verdict::Add);

    // Desired entry omits `fileMatch` entirely (treated as empty).
    let no_fm = serde_json::json!({
        "name": "Unrelated", "description": "d",
        "url": "https://tj-smith47.github.io/anodizer/schema.json"
    });
    assert_eq!(verdict(CATALOG, &no_fm).unwrap(), Verdict::Add);

    // A catalog entry whose own `fileMatch` is empty is likewise never matched
    // by a real desired glob under a different name.
    let cat = r#"{ "schemas": [
        { "name": "NoGlobs", "description": "x", "fileMatch": [], "url": "https://x/n.json" }
    ] }"#;
    let want = serde_json::json!({
        "name": "Other", "description": "y", "fileMatch": ["only.yaml"], "url": "https://x/o.json"
    });
    assert_eq!(verdict(cat, &want).unwrap(), Verdict::Add);
}

/// cfgd's merged upstream `cfgd Profile` entry was hand-written with NO
/// `fileMatch`, so nothing can overlap it. Keyed on `fileMatch` alone the
/// publisher read that as `Add` and appended a SECOND `cfgd Profile`, which
/// SchemaStore's `validate` rejects ("two schema entries with duplicate
/// name"). The name half of the identity rule makes it an `Update`, and the
/// splice rewrites the glob-less entry in place.
#[test]
fn glob_less_upstream_entry_is_updated_in_place_not_duplicated() {
    let upstream = serde_json::json!({
        "name": "cfgd Profile",
        "description": "cfgd profile file",
        "url": "https://www.schemastore.org/cfgd-profile-0.5.0.json",
        "versions": { "0.5.0": "https://www.schemastore.org/cfgd-profile-0.5.0.json" }
    });
    let catalog =
        serde_json::to_string_pretty(&serde_json::json!({ "schemas": [upstream] })).unwrap();

    let fm = vec!["*.cfgd-profile.yaml".to_string()];
    let mut versions = serde_json::Map::new();
    versions.insert(
        "0.10.0".into(),
        serde_json::json!("https://www.schemastore.org/cfgd-profile-0.10.0.json"),
    );
    let want = build_entry_json(
        "cfgd Profile",
        "cfgd profile file",
        &fm,
        "https://www.schemastore.org/cfgd-profile-0.10.0.json",
        Some(&versions),
    );

    assert_eq!(verdict(&catalog, &want).unwrap(), Verdict::Update);

    let out = splice_entry(&catalog, &want).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    let names: Vec<&str> = parsed["schemas"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e.get("name").and_then(serde_json::Value::as_str))
        .collect();
    assert_eq!(
        names,
        vec!["cfgd Profile"],
        "the glob-less upstream entry must be replaced, never duplicated"
    );
    assert_eq!(
        parsed["schemas"][0]["url"],
        "https://www.schemastore.org/cfgd-profile-0.10.0.json"
    );
}

/// The name half of the identity rule is case-insensitive: an upstream entry
/// renamed in review (`CFGD Profile`) and carrying no `fileMatch` still matches.
#[test]
fn glob_less_upstream_entry_matches_name_case_insensitively() {
    let catalog = r#"{ "schemas": [
        { "name": "CFGD Profile", "description": "cfgd profile file", "url": "https://www.schemastore.org/cfgd-profile-0.5.0.json" }
    ] }"#;
    let want = build_entry_json(
        "cfgd Profile",
        "cfgd profile file",
        &["*.cfgd-profile.yaml".to_string()],
        "https://www.schemastore.org/cfgd-profile-0.10.0.json",
        None,
    );
    assert_eq!(verdict(catalog, &want).unwrap(), Verdict::Update);

    let out = splice_entry(catalog, &want).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        parsed["schemas"].as_array().unwrap().len(),
        1,
        "a case-drifted glob-less entry must be replaced in place: {out}"
    );
}

/// The `versions` carry-forward reads the entry the splice will rewrite, so a
/// glob-less upstream entry's older versioned URLs survive the release. Keyed
/// on `fileMatch` alone the lookup missed it and the map was rebuilt from
/// scratch, orphaning `0.5.0` (SchemaStore CI then rejects the entry).
#[test]
fn versions_carry_forward_reads_the_name_matched_entry() {
    let catalog = r#"{ "schemas": [
        { "name": "cfgd Profile", "description": "cfgd profile file",
          "url": "https://www.schemastore.org/cfgd-profile-0.5.0.json",
          "versions": { "0.5.0": "https://www.schemastore.org/cfgd-profile-0.5.0.json" } }
    ] }"#;
    let prior = upstream_versions_for(
        catalog,
        "cfgd Profile",
        &["*.cfgd-profile.yaml".to_string()],
    )
    .expect("the name-matched entry must be found")
    .expect("well-formed catalog");
    assert_eq!(
        prior.get("0.5.0").unwrap(),
        "https://www.schemastore.org/cfgd-profile-0.5.0.json"
    );

    let merged = merge_versions(
        Some(&prior),
        "0.10.0",
        "https://www.schemastore.org/cfgd-profile-0.10.0.json",
    );
    assert_eq!(merged.len(), 2, "both versions must be listed: {merged:?}");
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
    let out = splice_entry(catalog, &entry).unwrap();
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
    let out = splice_entry(catalog, &entry).unwrap();
    assert!(out.contains("\"description\": \"new\""));
    assert!(!out.contains("\"description\": \"old\""));
    serde_json::from_str::<serde_json::Value>(&out).unwrap();
}

/// The upstream entry is title-case `Anodizer`; the desired entry is
/// lowercase `anodizer` with an overlapping `fileMatch`. `splice_entry` must
/// REPLACE the existing entry in place (matched by fileMatch) and NOT append a
/// second entry — the exact bug that tripped SchemaStore's duplicate-fileMatch
/// validator every release.
#[test]
fn splice_replaces_on_filematch_overlap_no_duplicate_append() {
    let catalog = "{\n  \"schemas\": [\n    {\n      \"name\": \"Anodizer\",\n      \"description\": \"old\",\n      \"fileMatch\": [\".anodizer.yaml\", \".anodizer.yml\"],\n      \"url\": \"https://u/old.json\"\n    }\n  ]\n}\n";
    let entry = serde_json::json!({ "name": "anodizer", "description": "new", "fileMatch": [".anodizer.yaml", ".anodizer.yml"], "url": "https://u/new.json" });
    let out = splice_entry(catalog, &entry).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let arr = v["schemas"].as_array().unwrap();
    assert_eq!(
        arr.len(),
        1,
        "must replace in place, never append a duplicate"
    );
    assert_eq!(arr[0]["name"], "anodizer");
    assert_eq!(arr[0]["description"], "new");
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
    let out = splice_entry(catalog, &entry).unwrap();
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
    let out = splice_entry(catalog, &entry).unwrap();
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
    let out = splice_entry(catalog, &entry).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let arr = v["schemas"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "First");
}

#[test]
fn merge_versions_carries_prior_and_adds_new() {
    let mut prior = serde_json::Map::new();
    prior.insert(
        "1.2".into(),
        serde_json::json!("https://www.schemastore.org/cfgd-config-1.2.json"),
    );
    let merged = merge_versions(
        Some(&prior),
        "1.3",
        "https://www.schemastore.org/cfgd-config-1.3.json",
    );
    assert_eq!(
        merged.get("1.2").unwrap(),
        "https://www.schemastore.org/cfgd-config-1.2.json"
    );
    assert_eq!(
        merged.get("1.3").unwrap(),
        "https://www.schemastore.org/cfgd-config-1.3.json"
    );
}

#[test]
fn adds_name_to_high_schema_version_array() {
    let jsonc = "{\n  // comment\n  \"highSchemaVersion\": [\n    \"existing-2020\"\n  ]\n}\n";
    let out = add_high_schema_version(jsonc, "cfgd-module").unwrap();
    assert!(out.contains("// comment"), "comments preserved");
    assert!(out.contains("\"existing-2020\""));
    assert!(out.contains("\"cfgd-module\""));
}

#[test]
fn idempotent_when_already_present() {
    let jsonc = "{\n  \"highSchemaVersion\": [\n    \"cfgd-module\"\n  ]\n}\n";
    let out = add_high_schema_version(jsonc, "cfgd-module").unwrap();
    assert_eq!(out.matches("\"cfgd-module\"").count(), 1);
}

/// A `//` comment line before the array contains both `]` and the key text,
/// and an existing element string contains a `]`. A naive `[`/`]` depth scan
/// (without comment- and string-skipping) would mislocate the array open/close
/// and splice in the wrong place. This proves the scanner ignores brackets and
/// key-text inside comments and string literals.
#[test]
fn add_high_schema_version_ignores_brackets_in_comments_and_strings() {
    let jsonc = "{\n  // note: highSchemaVersion ] array follows; do not be fooled ]\n  \"highSchemaVersion\": [\n    \"has-a-]-bracket\"\n  ]\n}\n";
    let out = add_high_schema_version(jsonc, "cfgd-module").unwrap();
    assert!(
        out.contains("// note: highSchemaVersion"),
        "comment survives"
    );
    assert!(
        out.contains("\"has-a-]-bracket\""),
        "string element survives"
    );
    assert!(out.contains("\"cfgd-module\""), "new element inserted");
    // Strip `//` comments, then re-parse to prove the splice produced valid
    // JSON with exactly one new element added to the array.
    let stripped: String = out
        .lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let v: serde_json::Value = serde_json::from_str(&stripped).unwrap();
    let arr = v["highSchemaVersion"].as_array().unwrap();
    assert_eq!(arr.len(), 2, "exactly one element added");
    assert!(arr.iter().any(|e| e == "has-a-]-bracket"));
    assert!(arr.iter().any(|e| e == "cfgd-module"));
}

/// `"cfgd-module-extra"` contains the bytes of `cfgd-module` but is a distinct
/// element. Idempotency must be element-exact, not substring — adding
/// `cfgd-module` to an array holding only `cfgd-module-extra` must insert.
#[test]
fn add_high_schema_version_distinguishes_prefix_elements() {
    let jsonc = "{\n  \"highSchemaVersion\": [\n    \"cfgd-module-extra\"\n  ]\n}\n";
    let out = add_high_schema_version(jsonc, "cfgd-module").unwrap();
    assert!(
        out.contains("\"cfgd-module-extra\""),
        "prefix element preserved"
    );
    // `"cfgd-module"` (with trailing quote) does NOT match inside
    // `"cfgd-module-extra"`, so this counts the standalone element only.
    assert_eq!(
        out.matches("\"cfgd-module\"").count(),
        1,
        "new element inserted"
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let arr = v["highSchemaVersion"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
}

/// A string value containing `//` (a URL) sits BEFORE the array and a
/// URL-looking element sits inside it. A scanner that did not track string
/// state would treat the `//` as a comment opener and desync, mislocating the
/// array. This directly exercises the new entry point against the most likely
/// real-world break (SchemaStore's file is full of `https://` URLs).
#[test]
fn add_high_schema_version_ignores_double_slash_in_string() {
    let jsonc = "{\n  \"someUrl\": \"https://schemastore.org/x.json\",\n  \"highSchemaVersion\": [\n    \"https://elem/y.json\"\n  ]\n}\n";
    let out = add_high_schema_version(jsonc, "cfgd-module").unwrap();
    assert!(
        out.contains("\"https://schemastore.org/x.json\""),
        "sibling URL preserved"
    );
    assert!(
        out.contains("\"https://elem/y.json\""),
        "URL element preserved"
    );
    assert_eq!(
        out.matches("\"cfgd-module\"").count(),
        1,
        "inserted exactly once"
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let arr = v["highSchemaVersion"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert!(arr.iter().any(|e| e == "cfgd-module"));
}

#[test]
fn add_high_schema_version_handles_empty_array() {
    let jsonc = "{\n  \"highSchemaVersion\": []\n}\n";
    let out = add_high_schema_version(jsonc, "cfgd-module").unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let arr = v["highSchemaVersion"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0], "cfgd-module");
    // Formatting is pinned, not just JSON-validity: the key line is indented 2
    // spaces, so the element lands at 4 and the closing `]` at 2 (prettier).
    assert!(
        out.contains("\n    \"cfgd-module\"\n  ]"),
        "element at key-indent+2, closing ] at key-indent; got:\n{out}"
    );
}

// --- rollback touches no catalog ---------------------------------------

/// SchemaStore rollback closes the registration PR and nothing else: it must
/// never edit `catalog.json`, because a name-matched upstream entry the publish
/// UPDATED in place was not created here, and unwinding it would delete
/// somebody else's registration.
///
/// Pinned two ways — running the rollback path leaves a catalog file's bytes
/// untouched, and the module carries no reference to the catalog editors at
/// all, so a future `catalog::splice_entry` / `upsert_schema_options` call in
/// `rollback.rs` trips this test rather than shipping.
#[test]
fn rollback_never_edits_the_catalog() {
    let dir = tempfile::tempdir().expect("temp dir");
    let catalog_path = dir.path().join("catalog.json");
    std::fs::write(&catalog_path, CATALOG).unwrap();

    let mut ctx = anodizer_core::test_helpers::TestContextBuilder::new().build();
    let evidence = anodizer_core::PublishEvidence::new("schemastore");
    crate::schemastore::rollback::rollback_publish(&mut ctx, &evidence).expect("rollback Ok");

    assert_eq!(
        std::fs::read_to_string(&catalog_path).unwrap(),
        CATALOG,
        "rollback must not rewrite a catalog file"
    );

    let src = include_str!("rollback.rs");
    for editor in ["catalog::", "splice_entry", "upsert_schema_options"] {
        assert!(
            !src.contains(editor),
            "rollback.rs must not reach for `{editor}` — closing the PR is the \
             whole unwind; editing the catalog would delete an upstream entry \
             the publish only updated"
        );
    }
}

// --- per-file validator `options` --------------------------------------
//
// The fixture models the shape SchemaStore's `src/schema-validation.jsonc`
// really has when cfgd is already registered: comments, a `highSchemaVersion`
// array, and an `options` map whose only member is the block the previous
// release's PR hand-wrote for `cfgd-config-0.5.0.json`.

const VALIDATION_JSONC: &str = r#"{
  // Only add here when the schema is not draft-07 compatible
  "highSchemaVersion": [
    "cfgd-config-0.5.0.json"
  ],
  "options": {
    "cfgd-config-0.5.0.json": {
      "unknownFormat": ["uint32"],
      "unknownKeywords": ["x-taplo"]
    }
  }
}
"#;

#[test]
fn upsert_schema_options_adds_a_block_for_a_new_file() {
    let mut block = serde_json::Map::new();
    block.insert("unknownFormat".into(), serde_json::json!(["uint32"]));
    let out = upsert_schema_options(VALIDATION_JSONC, "cfgd-config-0.10.0.json", &block).unwrap();

    assert!(
        out.contains("// Only add here when"),
        "comments must survive the textual splice; got:\n{out}"
    );
    let got = schema_options_block(&out, "cfgd-config-0.10.0.json").expect("new block present");
    assert_eq!(
        got.get("unknownFormat").unwrap(),
        &serde_json::json!(["uint32"])
    );
    assert!(
        schema_options_block(&out, "cfgd-config-0.5.0.json").is_some(),
        "the retained prior version's file keeps its block; got:\n{out}"
    );
    // Formatting is pinned: members sit at the existing member indent (4) and
    // the object closes at the key indent (2).
    assert!(
        out.contains("\n    \"cfgd-config-0.10.0.json\": {"),
        "new member at the existing member indent; got:\n{out}"
    );
}

#[test]
fn upsert_schema_options_rewrites_an_existing_block_in_place() {
    let mut block = serde_json::Map::new();
    block.insert(
        "unknownFormat".into(),
        serde_json::json!(["uint32", "uint64"]),
    );
    let out = upsert_schema_options(VALIDATION_JSONC, "cfgd-config-0.5.0.json", &block).unwrap();
    assert_eq!(
        out.matches("\"cfgd-config-0.5.0.json\": {").count(),
        1,
        "an existing block is rewritten, never duplicated; got:\n{out}"
    );
    let got = schema_options_block(&out, "cfgd-config-0.5.0.json").unwrap();
    assert_eq!(
        got.get("unknownFormat").unwrap(),
        &serde_json::json!(["uint32", "uint64"])
    );
}

#[test]
fn upsert_schema_options_handles_an_empty_options_object() {
    let jsonc = "{\n  \"options\": {}\n}\n";
    let mut block = serde_json::Map::new();
    block.insert("unknownFormat".into(), serde_json::json!(["uint32"]));
    let out = upsert_schema_options(jsonc, "cfgd-config-0.10.0.json", &block).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["options"]["cfgd-config-0.10.0.json"]["unknownFormat"],
        serde_json::json!(["uint32"]),
        "an empty options object takes no leading comma; got:\n{out}"
    );
}

/// A reviewer's `//` annotation inside an `options` block must not make the
/// block unreadable. `upsert_schema_options` REPLACES the whole member span,
/// so a block read as absent takes every sibling option (`unknownKeywords`,
/// `externalSchema`) down with it — which is what fails SchemaStore's CI.
#[test]
fn an_options_block_with_a_comment_still_reads() {
    let jsonc = r#"{
  "options": {
    "cfgd-config-0.5.0.json": {
      // keep: taplo emits this one
      "unknownKeywords": ["x-taplo"],
      "unknownFormat": ["uint32"], // and this format
      "note": "a // inside a string is data"
    }
  }
}
"#;
    let got = schema_options_block(jsonc, "cfgd-config-0.5.0.json")
        .expect("a commented block must still parse");
    assert_eq!(
        got.get("unknownKeywords").unwrap(),
        &serde_json::json!(["x-taplo"]),
        "the reviewer's sibling options survive the comment"
    );
    assert_eq!(
        got.get("unknownFormat").unwrap(),
        &serde_json::json!(["uint32"])
    );
    assert_eq!(
        got.get("note").unwrap(),
        &serde_json::json!("a // inside a string is data"),
        "a `//` inside a string literal is not a comment"
    );
}

/// A `//` comment naming `"options"` precedes the real key, one string value
/// spells it inside escaped quotes, and another IS the bare word. A plain
/// `text.find("\"options\"")` would anchor
/// on the comment and splice into the wrong object; matching any decoded
/// string would anchor on the value. Only a match in KEY position (followed by
/// `:`) is the map.
#[test]
fn options_key_is_located_past_comments_and_string_values() {
    let jsonc = r#"{
  // "options": { "decoy.json": { "unknownFormat": ["nope"] } } is NOT the map
  "note": "the word \"options\" appears here too",
  "kind": "options",
  "unrelated": { "not": "the map" },
  "options": {
    "cfgd-config-0.5.0.json": {
      "unknownFormat": ["uint32"]
    }
  }
}
"#;
    let got = schema_options_block(jsonc, "cfgd-config-0.5.0.json")
        .expect("the structural `options` key must be found past decoys");
    assert_eq!(
        got.get("unknownFormat").unwrap(),
        &serde_json::json!(["uint32"])
    );

    let mut block = serde_json::Map::new();
    block.insert("unknownFormat".into(), serde_json::json!(["uint64"]));
    let out = upsert_schema_options(jsonc, "cfgd-config-0.10.0.json", &block).unwrap();
    let v: serde_json::Value = serde_json::from_str(
        &out.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .expect("the splice must leave parseable JSONC");
    assert_eq!(
        v["options"]["cfgd-config-0.10.0.json"]["unknownFormat"],
        serde_json::json!(["uint64"]),
        "the new block must land in the real options map; got:\n{out}"
    );
    assert!(
        v["note"].is_string(),
        "the decoy string value must be untouched; got:\n{out}"
    );
}

/// The same decoy rule governs `highSchemaVersion`, whose key hunt shares the
/// scanner: a commented-out key must not capture the insert.
#[test]
fn high_schema_version_key_is_located_past_a_comment_decoy() {
    let jsonc = "{\n  // \"highSchemaVersion\": [ \"decoy.json\" ]\n  \"highSchemaVersion\": [\n    \"real.json\"\n  ]\n}\n";
    let out = add_high_schema_version(jsonc, "cfgd-module.json").unwrap();
    let v: serde_json::Value = serde_json::from_str(
        &out.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .expect("the splice must leave parseable JSONC");
    let arr = v["highSchemaVersion"].as_array().unwrap();
    assert_eq!(arr.len(), 2, "insert into the real array; got:\n{out}");
    assert_eq!(arr[1], "cfgd-module.json");
}

#[test]
fn schema_options_block_is_absent_for_an_unlisted_file() {
    assert!(schema_options_block(VALIDATION_JSONC, "cfgd-config-0.10.0.json").is_none());
    assert!(schema_options_block("{ \"highSchemaVersion\": [] }", "any.json").is_none());
}

// --- unknown `format` derivation ---------------------------------------

#[test]
fn unknown_formats_finds_formats_at_any_depth() {
    let schema = serde_json::json!({
        "$defs": {
            "port": { "type": "integer", "format": "uint32" },
            "list": { "items": [{ "format": "uint16" }] }
        },
        "properties": { "when": { "type": "string", "format": "date-time" } }
    });
    assert_eq!(unknown_formats(&schema), vec!["uint16", "uint32"]);
}

/// SchemaStore's `cli.js` registers `@hyperupcall/ajv-formats-draft2019`
/// alongside `ajv-formats`, so the four draft-2019-09 string formats resolve
/// and must NOT be declared as unknown.
#[test]
fn unknown_formats_empty_for_the_draft2019_formats() {
    let schema = serde_json::json!({
        "properties": {
            "a": { "format": "iri" },
            "b": { "format": "iri-reference" },
            "c": { "format": "idn-email" },
            "d": { "format": "idn-hostname" }
        }
    });
    assert!(
        unknown_formats(&schema).is_empty(),
        "ajv-formats-draft2019 registers all four: {:?}",
        unknown_formats(&schema)
    );
}

/// A `"format"` key inside a data-carrying subtree (`default`, `const`,
/// `examples`, `enum`) is a sample VALUE, not a declaration. Collecting it
/// would emit a bogus `unknownFormat` entry for a format the schema never uses.
#[test]
fn unknown_formats_ignores_data_carrying_subtrees() {
    let schema = serde_json::json!({
        "properties": {
            "out": {
                "type": "object",
                "default": { "format": "bogus" },
                "const": { "format": "alsobogus" },
                "examples": [{ "format": "examplebogus" }],
                "enum": [{ "format": "enumbogus" }]
            }
        }
    });
    assert!(
        unknown_formats(&schema).is_empty(),
        "sample values must not become format declarations; got {:?}",
        unknown_formats(&schema)
    );

    // A real declaration beside the data subtrees is still collected.
    let mixed = serde_json::json!({
        "properties": {
            "port": { "type": "integer", "format": "uint32", "default": { "format": "bogus" } }
        }
    });
    assert_eq!(unknown_formats(&mixed), vec!["uint32"]);
}

#[test]
fn unknown_formats_empty_when_every_format_is_known_to_ajv() {
    let schema = serde_json::json!({
        "properties": {
            "when": { "format": "date-time" },
            "who": { "format": "email" },
            "where": { "format": "uri" },
            "id": { "format": "uuid" }
        }
    });
    assert!(
        unknown_formats(&schema).is_empty(),
        "ajv-formats knows every one of these"
    );
}

#[test]
fn jsonc_array_contains_finds_element_ignoring_comments() {
    let jsonc = "{\n  // dialect allowlist\n  \"highSchemaVersion\": [\n    \"cfgd-module.json\",\n    \"other.json\"\n  ]\n}\n";
    assert!(jsonc_array_contains(
        jsonc,
        "highSchemaVersion",
        "cfgd-module.json"
    ));
    assert!(jsonc_array_contains(
        jsonc,
        "highSchemaVersion",
        "other.json"
    ));
}

#[test]
fn jsonc_array_contains_is_element_exact_not_substring() {
    let jsonc = "{\n  \"highSchemaVersion\": [\n    \"cfgd-module-extra.json\"\n  ]\n}\n";
    assert!(!jsonc_array_contains(
        jsonc,
        "highSchemaVersion",
        "cfgd-module.json"
    ));
}

#[test]
fn jsonc_array_contains_missing_key_is_false_not_error() {
    // A catalog/jsonc lacking the key must read as "not a member" — the
    // conservative direction for the schemastore change-decision.
    let jsonc = "{\n  \"other\": []\n}\n";
    assert!(!jsonc_array_contains(jsonc, "highSchemaVersion", "x.json"));
}

#[test]
fn merge_versions_from_empty() {
    let merged = merge_versions(None, "1.0.0", "https://www.schemastore.org/x-1.0.0.json");
    assert_eq!(merged.len(), 1);
    assert_eq!(
        merged.get("1.0.0").unwrap(),
        "https://www.schemastore.org/x-1.0.0.json"
    );
}

#[test]
fn format_vendor_schema_is_2space_with_trailing_newline() {
    let raw = "{\"$schema\":\"http://json-schema.org/draft-07/schema#\",\"type\":\"object\"}";
    let out = format_vendor_schema(raw).unwrap();
    assert!(out.ends_with("}\n"));
    assert!(out.contains("\n  \"type\": \"object\""));
}

#[test]
fn format_vendor_schema_preserves_key_order() {
    // Non-alphabetical order: type, $schema, title — a sorting serializer would
    // emit $schema, title, type (alphabetical). Proves preserve_order is in effect.
    let raw = "{\"type\":\"object\",\"$schema\":\"http://json-schema.org/draft-07/schema#\",\"title\":\"X\"}";
    let out = format_vendor_schema(raw).unwrap();
    let type_pos = out.find("\"type\"").unwrap();
    let schema_pos = out.find("\"$schema\"").unwrap();
    let title_pos = out.find("\"title\"").unwrap();
    assert!(
        type_pos < schema_pos && schema_pos < title_pos,
        "key order must be preserved (type < $schema < title); got:\n{out}"
    );
}

/// The SchemaStore publish always lands as a PR against
/// `SchemaStore/schemastore`; `gh pr create` is the preferred transport with
/// a full REST-API fallback, so `gh` is ADVISORY — recommended, never a
/// blocker.
#[test]
fn schemastore_advisory_requirements_emit_gh_when_active() {
    use anodizer_core::Publisher;
    use anodizer_core::config::{Config, SchemaEntry, SchemastoreConfig};
    use anodizer_core::context::{Context, ContextOptions};
    let config = Config {
        schemastore: SchemastoreConfig {
            schemas: vec![SchemaEntry {
                name: "Anodizer".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let ctx = Context::new(config, ContextOptions::default());
    let reqs = crate::schemastore::SchemastorePublisher::new().advisory_requirements(&ctx);
    assert!(
        reqs.iter().any(|r| matches!(
            r,
            anodizer_core::EnvRequirement::Tool { name } if name == "gh"
        )),
        "active schemastore config must recommend gh: {reqs:?}"
    );
}

#[test]
fn schemastore_advisory_requirements_empty_when_skipped() {
    use anodizer_core::Publisher;
    use anodizer_core::config::{Config, SchemaEntry, SchemastoreConfig, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let config = Config {
        schemastore: SchemastoreConfig {
            skip: Some(StringOrBool::Bool(true)),
            schemas: vec![SchemaEntry {
                name: "Anodizer".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let ctx = Context::new(config, ContextOptions::default());
    let reqs = crate::schemastore::SchemastorePublisher::new().advisory_requirements(&ctx);
    assert!(
        reqs.is_empty(),
        "a skipped schemastore config must recommend nothing: {reqs:?}"
    );
}
